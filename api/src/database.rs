use anyhow::anyhow;
use aws_config::timeout;
use aws_sdk_rds::{error::ModifyDBInstanceErrorKind, types::SdkError};
use aws_smithy_types::tristate::TriState;
use lazy_static::lazy_static;
use rand::Rng;
use shuttle_common::{project::ProjectName, DatabaseReadyInfo};
use shuttle_service::error::CustomError;
use sqlx::postgres::{PgPool, PgPoolOptions};
use std::time::Duration;
use tokio::time::sleep;

lazy_static! {
    static ref SUDO_POSTGRES_CONNECTION_STRING: String = format!(
        "postgres://postgres:{}@localhost",
        std::env::var("PG_PASSWORD").expect(
            "superuser postgres role password expected as environment variable PG_PASSWORD"
        )
    );
}

fn generate_role_password() -> String {
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(12)
        .map(char::from)
        .collect()
}

pub(crate) struct State {
    project: ProjectName,
    context: Context,
    info: Option<DatabaseReadyInfo>,
}

impl State {
    pub(crate) fn new(project: &ProjectName, context: &Context) -> Self {
        Self {
            project: project.clone(),
            context: context.clone(),
            info: None,
        }
    }

    pub(crate) async fn request(&mut self) -> sqlx::Result<DatabaseReadyInfo> {
        if self.info.is_some() {
            return Ok(self.info.clone().unwrap());
        }

        let role_name = format!("user-{}", self.project);
        let role_password = generate_role_password();
        let database_name = format!("db-{}", self.project);

        let pool = &self.context.sudo_pool;

        // Check if this deployment already has its own role:
        let rows = sqlx::query("SELECT * FROM pg_roles WHERE rolname = $1")
            .bind(&role_name)
            .fetch_all(pool)
            .await?;

        if rows.is_empty() {
            // Create role if it does not already exist:
            // TODO: Should be able to use `.bind` instead of `format!` but doesn't seem to
            // insert quotes correctly.
            let create_role_query = format!(
                "CREATE ROLE \"{}\" PASSWORD '{}' LOGIN",
                role_name, role_password
            );
            sqlx::query(&create_role_query).execute(pool).await?;

            debug!(
                "created new role '{}' in database for project '{}'",
                role_name, database_name
            );
        } else {
            // If the role already exists then change its password:
            let alter_password_query = format!(
                "ALTER ROLE \"{}\" WITH PASSWORD '{}'",
                role_name, role_password
            );
            sqlx::query(&alter_password_query).execute(pool).await?;

            debug!(
                "role '{}' already exists so updating their password",
                role_name
            );
        }

        // Since user creation is not atomic, need to separately check for DB existence
        let get_database_query = "SELECT 1 FROM pg_database WHERE datname = $1";
        let database = sqlx::query(get_database_query)
            .bind(&database_name)
            .fetch_all(pool)
            .await?;
        if database.is_empty() {
            debug!("database '{}' does not exist, creating", database_name);
            // Create the database (owned by the new role):
            let create_database_query = format!(
                "CREATE DATABASE \"{}\" OWNER '{}'",
                database_name, role_name
            );
            sqlx::query(&create_database_query).execute(pool).await?;

            debug!(
                "created database '{}' belonging to '{}'",
                database_name, role_name
            );
        } else {
            debug!(
                "database '{}' already exists, not recreating",
                database_name
            );
        }

        let info = DatabaseReadyInfo::new(role_name, role_password, database_name);
        self.info = Some(info.clone());
        Ok(info)
    }

    pub(crate) fn to_info(&self) -> Option<DatabaseReadyInfo> {
        self.info.clone()
    }

    pub(crate) async fn aws_rds(&self) -> Result<String, shuttle_service::Error> {
        let client = &self.context.rds_client;

        let username = self.project.to_string().replace("-", "_");
        let password = generate_role_password();
        let engine = "postgres";
        let class = "db.t3.micro";
        let instance_name = format!("{}-{}", self.project, engine);
        let db_name = "postgres";

        let instances = client
            .modify_db_instance()
            .db_instance_identifier(&instance_name)
            .master_user_password(&password)
            .send()
            .await;
        debug!("got describe response");

        let mut instance = match instances {
            Ok(instances) => instances.db_instance.unwrap().clone(),
            Err(SdkError::ServiceError { err, .. }) => {
                if let ModifyDBInstanceErrorKind::DbInstanceNotFoundFault(_) = err.kind {
                    debug!("creating new");

                    client
                        .create_db_instance()
                        .db_instance_identifier(&instance_name)
                        .master_username(username)
                        .master_user_password(&password)
                        .engine(engine)
                        .db_instance_class(class)
                        .allocated_storage(20)
                        .backup_retention_period(0)
                        .publicly_accessible(true)
                        .db_name(db_name)
                        .send()
                        .await
                        .map_err(shuttle_service::error::CustomError::new)?
                        .db_instance
                        .unwrap()
                } else {
                    return Err(shuttle_service::Error::Custom(anyhow!(
                        "got unexpected error from AWS: {}",
                        err
                    )));
                }
            }
            Err(unexpected) => {
                return Err(shuttle_service::Error::Custom(anyhow!(
                    "got unexpected error from AWS: {}",
                    unexpected
                )))
            }
        };

        // Wait for up
        debug!("waiting for password update");
        sleep(Duration::from_secs(30)).await;
        loop {
            instance = client
                .describe_db_instances()
                .db_instance_identifier(&instance_name)
                .send()
                .await
                .map_err(CustomError::new)?
                .db_instances
                .unwrap()
                .get(0)
                .unwrap()
                .clone();

            let status = instance.db_instance_status.as_ref().unwrap().clone();

            debug!("status: {status}");
            if status == "available" {
                break;
            }
            sleep(Duration::from_secs(1)).await;
        }

        println!("{instance:#?}");
        // let info = DatabaseReadyInfo::new(role_name, role_password, database_name);
        let conn_string = format!(
            "postgres://{}:{}@{}/{}",
            instance.master_username.unwrap(),
            password,
            instance.endpoint.unwrap().address.unwrap(),
            db_name
        );

        Ok(conn_string)
    }
}

#[derive(Clone)]
pub struct Context {
    sudo_pool: PgPool,
    rds_client: aws_sdk_rds::Client,
}

impl Context {
    pub async fn new() -> sqlx::Result<Self> {
        let sudo_pool = PgPoolOptions::new()
            .min_connections(4)
            .max_connections(12)
            .connect_timeout(Duration::from_secs(60))
            .connect_lazy(&SUDO_POSTGRES_CONNECTION_STRING)?;

        let api_timeout_config =
            timeout::Api::new().with_call_timeout(TriState::Set(Duration::from_secs(5)));
        let timeout_config = timeout::Config::new().with_api_timeouts(api_timeout_config);
        let aws_config = aws_config::from_env()
            .timeout_config(timeout_config)
            .load()
            .await;

        let rds_client = aws_sdk_rds::Client::new(&aws_config);

        Ok(Self {
            sudo_pool,
            rds_client,
        })
    }
}
