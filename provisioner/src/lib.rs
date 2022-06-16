use std::time::Duration;

pub use args::Args;
use aws_config::timeout;
use aws_sdk_rds::{error::ModifyDBInstanceErrorKind, model::DbInstance, types::SdkError, Client};
use aws_smithy_types::tristate::TriState;
pub use error::Error;
use proto::provisioner::provisioner_server::Provisioner;
pub use proto::provisioner::provisioner_server::ProvisionerServer;
use proto::provisioner::{
    aws_rds, database_request::DbType, AwsRds, DatabaseRequest, DatabaseResponse,
};
use rand::Rng;
use sqlx::{postgres::PgPoolOptions, PgPool};
use tokio::time::sleep;
use tonic::{Request, Response, Status};
use tracing::{debug, info};

mod args;
mod error;

const PRIVATE_PG_IP: &str = "provisioner";
const PUBLIC_PG_IP: &str = "pg.shuttle.rs";

const AWS_RDS_CLASS: &str = "db.t4g.micro";
const MASTER_USERNAME: &str = "master";
const RDS_SUBNET_GROUP: &str = "shuttle_rds";

pub struct MyProvisioner {
    pool: PgPool,
    rds_client: aws_sdk_rds::Client,
}

impl MyProvisioner {
    pub async fn new(uri: &str) -> sqlx::Result<Self> {
        let pool = PgPoolOptions::new()
            .min_connections(4)
            .max_connections(12)
            .connect_timeout(Duration::from_secs(60))
            .connect_lazy(uri)?;

        // Default timeout is too long so lowering it
        let api_timeout_config = timeout::Api::new()
            .with_call_timeout(TriState::Set(Duration::from_secs(120)))
            .with_call_attempt_timeout(TriState::Set(Duration::from_secs(120)));
        let timeout_config = timeout::Config::new().with_api_timeouts(api_timeout_config);

        let aws_config = aws_config::from_env()
            .timeout_config(timeout_config)
            .load()
            .await;

        let rds_client = aws_sdk_rds::Client::new(&aws_config);

        Ok(Self { pool, rds_client })
    }

    pub async fn request_shared_db(&self, project_name: &str) -> Result<DatabaseResponse, Error> {
        let (username, password) = self.shared_role(project_name).await?;
        let database_name = self.shared_db(project_name, &username).await?;

        Ok(DatabaseResponse {
            engine: "postgres".to_string(),
            username,
            password,
            database_name,
            address_private: PRIVATE_PG_IP.to_string(),
            address_public: PUBLIC_PG_IP.to_string(),
            port: "3306".to_string(),
        })
    }

    async fn shared_role(&self, project_name: &str) -> Result<(String, String), Error> {
        let username = format!("user-{project_name}");
        let password = generate_password();

        let matching_user = sqlx::query("SELECT rolname FROM pg_roles WHERE rolname = $1")
            .bind(&username)
            .fetch_optional(&self.pool)
            .await?;

        if matching_user.is_none() {
            info!("creating new user");

            // Binding does not work for identifiers
            // https://stackoverflow.com/questions/63723236/sql-statement-to-create-role-fails-on-postgres-12-using-dapper
            let create_role_query =
                format!("CREATE ROLE \"{username}\" WITH LOGIN PASSWORD '{password}'");
            sqlx::query(&create_role_query)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::CreateRole(e.to_string()))?;
        } else {
            info!("cycling password of user");

            // Binding does not work for identifiers
            // https://stackoverflow.com/questions/63723236/sql-statement-to-create-role-fails-on-postgres-12-using-dapper
            let update_role_query =
                format!("ALTER ROLE \"{username}\" WITH LOGIN PASSWORD '{password}'");
            sqlx::query(&update_role_query)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::UpdateRole(e.to_string()))?;
        }

        Ok((username, password))
    }

    async fn shared_db(&self, project_name: &str, username: &str) -> Result<String, Error> {
        let database_name = format!("db-{project_name}");

        let matching_db = sqlx::query("SELECT datname FROM pg_database WHERE datname = $1")
            .bind(&database_name)
            .fetch_optional(&self.pool)
            .await?;

        if matching_db.is_none() {
            info!("creating database");

            // Binding does not work for identifiers
            // https://stackoverflow.com/questions/63723236/sql-statement-to-create-role-fails-on-postgres-12-using-dapper
            let create_db_query = format!("CREATE DATABASE \"{database_name}\" OWNER '{username}'");
            sqlx::query(&create_db_query)
                .execute(&self.pool)
                .await
                .map_err(|e| Error::CreateDB(e.to_string()))?;
        }

        Ok(database_name)
    }

    async fn request_aws_rds(
        &self,
        project_name: &str,
        engine: aws_rds::Engine,
    ) -> Result<DatabaseResponse, Error> {
        let client = &self.rds_client;

        let password = generate_password();
        let instance_name = format!("{}-{}", project_name, engine);

        debug!("trying to get AWS RDS instance: {instance_name}");
        let instance = client
            .modify_db_instance()
            .db_instance_identifier(&instance_name)
            .master_user_password(&password)
            .send()
            .await;

        match instance {
            Ok(_) => {
                wait_for_instance(client, &instance_name, "resetting-master-credentials").await?;
            }
            Err(SdkError::ServiceError { err, .. }) => {
                if let ModifyDBInstanceErrorKind::DbInstanceNotFoundFault(_) = err.kind {
                    debug!("creating new AWS RDS {instance_name}");

                    client
                        .create_db_instance()
                        .db_instance_identifier(&instance_name)
                        .master_username(MASTER_USERNAME)
                        .master_user_password(&password)
                        .engine(engine.to_string())
                        .db_instance_class(AWS_RDS_CLASS)
                        .allocated_storage(20)
                        .backup_retention_period(0) // Disable backups
                        .publicly_accessible(true)
                        .db_name(engine.to_string())
                        .set_db_subnet_group_name(Some(RDS_SUBNET_GROUP.to_string()))
                        .send()
                        .await?
                        .db_instance
                        .expect("to be able to create instance");

                    wait_for_instance(client, &instance_name, "creating").await?;
                } else {
                    return Err(Error::Plain(format!(
                        "got unexpected error from AWS RDS service: {}",
                        err
                    )));
                }
            }
            Err(unexpected) => {
                return Err(Error::Plain(format!(
                    "got unexpected error from AWS during API call: {}",
                    unexpected
                )))
            }
        };

        // Wait for up
        let instance = wait_for_instance(client, &instance_name, "available").await?;

        // TODO: find private IP somehow
        let address = instance
            .endpoint
            .expect("instance to have an endpoint")
            .address
            .expect("endpoint to have an address");

        Ok(DatabaseResponse {
            engine: engine.to_string(),
            username: instance
                .master_username
                .expect("instance to have a username"),
            password,
            database_name: instance
                .db_name
                .expect("instance to have a default database"),
            address_private: address.clone(),
            address_public: address,
            port: engine_to_port(engine),
        })
    }
}

#[tonic::async_trait]
impl Provisioner for MyProvisioner {
    #[tracing::instrument(skip(self))]
    async fn provision_database(
        &self,
        request: Request<DatabaseRequest>,
    ) -> Result<Response<DatabaseResponse>, Status> {
        let request = request.into_inner();
        let db_type = request.db_type.unwrap();

        let reply = match db_type {
            DbType::Shared(_) => self.request_shared_db(&request.project_name).await?,
            DbType::AwsRds(AwsRds { engine }) => {
                self.request_aws_rds(&request.project_name, engine.expect("oneof to be set"))
                    .await?
            }
        };

        Ok(Response::new(reply))
    }
}

fn generate_password() -> String {
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(12)
        .map(char::from)
        .collect()
}

async fn wait_for_instance(
    client: &Client,
    name: &str,
    wait_for: &str,
) -> Result<DbInstance, Error> {
    debug!("waiting for {name} to enter {wait_for} state");
    loop {
        let instance = client
            .describe_db_instances()
            .db_instance_identifier(name)
            .send()
            .await?
            .db_instances
            .expect("aws to return instances")
            .get(0)
            .expect("to find the instance just created or modified")
            .clone();

        let status = instance
            .db_instance_status
            .as_ref()
            .expect("instance to have a status")
            .clone();

        if status == wait_for {
            return Ok(instance);
        }

        sleep(Duration::from_secs(1)).await;
    }
}

fn engine_to_port(engine: aws_rds::Engine) -> String {
    match engine {
        aws_rds::Engine::Postgres(_) => "5432".to_string(),
        aws_rds::Engine::Mariadb(_) => "3306".to_string(),
        aws_rds::Engine::Mysql(_) => "3306".to_string(),
    }
}
