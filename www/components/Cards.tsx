import ExternalLink from "./ExternalLink";

interface Card {
  readonly title: string
  readonly description: string
  readonly link: string
  readonly icon: string
}

const cards: Card[] = [
  {
    title: 'Url Shortener',
    description: 'A URL shortener built with shuttle, rocket and postgres/sqlx. you can use it from your terminal.',
    link: 'https://github.com/getsynth/shuttle/pull/94/files',
    icon: '/images/icon1.svg',
  },
  {
    title: 'Url Shortener',
    description: 'A URL shortener built with shuttle, rocket and postgres/sqlx. you can use it from your terminal.',
    link: 'https://github.com/getsynth/shuttle/pull/94/files',
    icon: '/images/icon2.svg',
  },
  {
    title: 'Url Shortener',
    description: 'A URL shortener built with shuttle, rocket and postgres/sqlx. you can use it from your terminal.',
    link: 'https://github.com/getsynth/shuttle/pull/94/files',
    icon: '/images/icon3.svg',
  }
]


export default function Cards() {
  return (
    <div className="relative bg-dark-700 pt-16 pb-20 px-4 sm:px-6 lg:pt-24 lg:pb-28 lg:px-8">
      <div className="relative max-w-6xl mx-auto">
        <div className="text-center">
          <h2 className="text-3xl tracking-tight font-extrabold text-gray-200 sm:text-4xl">From the blog</h2>
          <p className="mt-3 max-w-2xl mx-auto text-xl text-gray-300 sm:mt-4">
            Lorem ipsum dolor sit amet consectetur, adipisicing elit. Ipsa libero labore natus atque, ducimus sed.
          </p>
        </div>
        <div className="mt-12  mx-auto grid gap-5 lg:grid-cols-3 w-fit">
          {cards.map((card, index) => (
            <ExternalLink href={card.link} key={index} className="flex flex-col rounded-lg hover:shadow-2xl hover:-translate-y-2 transition overflow-hidden max-w-sm ">
              <div className="flex-shrink-0 bg-dark-800">
                <img className="w-full object-contain p-14 aspect-[4/3]" src={card.icon} role="presentation" />
              </div>
              <div className="flex-1 bg-gray-500 p-6 flex flex-col justify-between">
                <div className="flex-1">
                  <div className="block mt-2">
                    <p className="text-xl font-semibold text-gray-200">{card.title}</p>
                    <p className="mt-3 text-base text-gray-300">{card.description}</p>
                  </div>
                </div>
              </div>
            </ExternalLink>
          ))}
        </div>
      </div>
    </div>
  )
}