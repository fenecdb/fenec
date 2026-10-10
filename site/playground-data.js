// The playground's data: what its database is seeded with, and the
// queries its Examples list holds -- a module of its own, so the tests
// (studio/test/local.test.mjs) seed the same database and run every
// example against it.

// ------------------------------------------------------------------ seed
//
// Three collections a visitor can read at once and join in their head:
// products with a description to search by words and a small vector to
// search by meaning, orders that point at products, and a week of a shop's
// events to count by the hour. Everything is made here, the same every
// time, so an example's answer is the one its comment describes.

/**
 * The axes of a product's vector, what it is for: a vector of eight is
 * small enough to type, and a query vector reads as a sentence --
 * `[0.9, 0, 0, 0, 0, 0.5, 0, 0]` is "coffee, on the road".
 */
export const AXES = ['coffee', 'tea', 'cooking', 'baking', 'outdoors', 'travel', 'reading', 'sound'];

// [name, category, price, rating, tags, description, {axis: weight}]: the
// category's axis weighs 1, the others what the product is also for.
const PRODUCTS = [
  ['Burr grinder', 'coffee', 129, 4.7, ['grinder', 'electric'], 'Conical steel burrs grind coffee evenly from espresso fine to French press coarse.', {}],
  ['Hand grinder', 'coffee', 59, 4.5, ['grinder', 'manual'], 'A hand grinder with ceramic burrs, quiet enough for the first cup of the day.', { travel: 0.4 }],
  ['Pour over dripper', 'coffee', 24, 4.6, ['brewing', 'manual'], 'A ceramic cone for pour over coffee, one cup at a time, bright and clean.', {}],
  ['Gooseneck kettle', 'coffee', 69, 4.8, ['kettle', 'brewing'], 'A kettle with a slow gooseneck spout and a thermometer for pour over coffee and tea.', { tea: 0.7 }],
  ['Espresso tamper', 'coffee', 32, 4.2, ['espresso', 'tools'], 'A flat steel tamper that presses espresso evenly into the basket.', {}],
  ['Cold brew jar', 'coffee', 28, 4.3, ['brewing', 'cold'], 'A glass jar with a fine filter: coffee steeped overnight in the fridge.', { outdoors: 0.3 }],
  ['Travel coffee press', 'coffee', 39, 4.4, ['brewing', 'travel'], 'A press and a mug in one: coffee on the road, on a trail or at a desk.', { travel: 0.8, outdoors: 0.4 }],
  ['Single origin beans', 'coffee', 18, 4.6, ['beans'], 'Washed Ethiopian beans, roasted light: jasmine, lemon and black tea.', { tea: 0.2 }],
  ['Loose leaf sampler', 'tea', 26, 4.5, ['leaves'], 'Six loose leaf teas, from a grassy sencha to a smoky lapsang.', {}],
  ['Glass teapot', 'tea', 34, 4.4, ['teapot', 'brewing'], 'A heat proof glass teapot with a steel infuser to watch the leaves open.', {}],
  ['Matcha whisk set', 'tea', 31, 4.6, ['matcha', 'tools'], 'A bamboo whisk, a scoop and a bowl for whisking matcha to a foam.', {}],
  ['Tea tumbler', 'tea', 22, 4.1, ['travel', 'brewing'], 'A double walled tumbler with a strainer: tea brewed on the way to work.', { travel: 0.7, coffee: 0.2 }],
  ['Cast iron kettle', 'tea', 74, 4.7, ['kettle', 'teapot'], 'A Japanese cast iron kettle that keeps tea hot through a long afternoon.', { cooking: 0.2 }],
  ['Herbal blends', 'tea', 15, 4.2, ['leaves', 'caffeine free'], 'Mint, chamomile and rooibos, caffeine free, for the evening.', { reading: 0.3 }],
  ['Chef knife', 'kitchen', 89, 4.8, ['knife', 'steel'], 'An eight inch chef knife of carbon steel that keeps its edge.', {}],
  ['Cast iron skillet', 'kitchen', 45, 4.7, ['pan', 'cast iron'], 'A seasoned cast iron skillet for searing, frying and campfire cooking.', { outdoors: 0.5 }],
  ['Spice grinder', 'kitchen', 27, 4.3, ['grinder', 'spices'], 'A small electric grinder for whole spices, and coffee beans at a pinch.', { coffee: 0.4 }],
  ['Cutting board', 'kitchen', 38, 4.5, ['wood'], 'An end grain walnut board that is kind to a knife edge.', {}],
  ['Dutch oven', 'kitchen', 99, 4.8, ['pot', 'cast iron'], 'An enamelled cast iron pot for stews and for baking bread.', { baking: 0.6 }],
  ['Kitchen scale', 'kitchen', 25, 4.4, ['scale', 'tools'], 'A scale to the gram for recipes, bread dough and brewing coffee.', { baking: 0.6, coffee: 0.4 }],
  ['Stand mixer', 'baking', 299, 4.7, ['mixer', 'electric'], 'A stand mixer that kneads bread dough and whips cream.', { cooking: 0.3 }],
  ['Sourdough starter kit', 'baking', 29, 4.5, ['bread', 'kit'], 'A jar, a band and a dried starter: everything for sourdough bread.', {}],
  ['Bread proofing basket', 'baking', 19, 4.6, ['bread'], 'A rattan banneton that gives a loaf its rings as it proofs.', {}],
  ['Silicone baking mats', 'baking', 16, 4.3, ['mats'], 'Two mats in place of parchment, for cookies and roasting.', { cooking: 0.3 }],
  ['Rolling pin', 'baking', 21, 4.4, ['wood', 'tools'], 'A French rolling pin of maple, tapered at the ends, for pastry.', {}],
  ['Loaf pan', 'baking', 23, 4.2, ['pan', 'bread'], 'A heavy steel loaf pan for sandwich bread and banana cake.', {}],
  ['Camp stove', 'outdoor', 79, 4.6, ['stove', 'camping'], 'A small gas stove that boils a litre of water in three minutes.', { cooking: 0.5, travel: 0.4 }],
  ['Trail mug', 'outdoor', 18, 4.3, ['mug', 'camping'], 'A titanium mug for coffee or tea at the top of the hill.', { coffee: 0.4, tea: 0.4 }],
  ['Headlamp', 'outdoor', 35, 4.5, ['light', 'camping'], 'A headlamp of 400 lumens for night walks and reading in the tent.', { reading: 0.3 }],
  ['Hammock', 'outdoor', 55, 4.7, ['camping', 'rest'], 'A parachute nylon hammock that packs to the size of a grapefruit.', { travel: 0.4, reading: 0.4 }],
  ['Water filter', 'outdoor', 42, 4.6, ['water', 'camping'], 'A squeeze filter for stream water, good for a thousand litres.', { travel: 0.5 }],
  ['Folding chair', 'outdoor', 64, 4.4, ['camping', 'rest'], 'A light folding chair for campsites, beaches and concerts.', { sound: 0.2 }],
  ['Carry-on backpack', 'travel', 149, 4.7, ['bag'], 'A forty litre backpack that fits the cabin and opens like a suitcase.', { outdoors: 0.4 }],
  ['Packing cubes', 'travel', 29, 4.5, ['bag', 'organizer'], 'Four zipped cubes that keep a week of clothes in order.', {}],
  ['Neck pillow', 'travel', 24, 4.0, ['rest'], 'A memory foam pillow for long flights and night trains.', { reading: 0.2 }],
  ['Universal adapter', 'travel', 27, 4.4, ['electric'], 'One adapter for 150 countries, with two USB ports.', { sound: 0.2 }],
  ['Travel journal', 'travel', 17, 4.6, ['paper', 'writing'], 'A notebook of dot grid paper with a pocket for tickets.', { reading: 0.6 }],
  ['Luggage scale', 'travel', 14, 4.2, ['scale'], 'A hand held scale so the bag is under the limit before the airport.', {}],
  ['The Little Prince', 'books', 12, 4.9, ['novel', 'classic'], 'A pilot meets a prince from a small planet in the desert, and a fox.', { travel: 0.3 }],
  ['Dune', 'books', 16, 4.8, ['novel', 'science fiction'], 'A desert planet, its spice, and the people who live on its sands.', { outdoors: 0.2 }],
  ['Salt Fat Acid Heat', 'books', 30, 4.8, ['cooking'], 'Four elements of good cooking, explained and drawn.', { cooking: 0.8 }],
  ['The World Atlas of Coffee', 'books', 28, 4.7, ['coffee'], 'Where coffee grows, how it is processed, and how to brew it.', { coffee: 0.8 }],
  ['Flour Water Salt Yeast', 'books', 25, 4.8, ['baking', 'bread'], 'Bread and pizza from a bakery in Portland, step by step.', { baking: 0.8 }],
  ['A Walk in the Woods', 'books', 14, 4.5, ['travel', 'memoir'], 'Two men set out to walk the Appalachian Trail.', { outdoors: 0.6, travel: 0.5 }],
  ['Noise cancelling headphones', 'audio', 249, 4.7, ['headphones', 'wireless'], 'Headphones that quiet a plane cabin, thirty hours a charge.', { travel: 0.6 }],
  ['Bookshelf speakers', 'audio', 199, 4.6, ['speakers'], 'A pair of small speakers with a warm sound for a living room.', { reading: 0.2 }],
  ['Portable speaker', 'audio', 79, 4.4, ['speakers', 'wireless'], 'A waterproof speaker for the beach, the campsite and the shower.', { outdoors: 0.6 }],
  ['Turntable', 'audio', 229, 4.5, ['vinyl'], 'A belt drive turntable with a built in preamp.', {}],
  ['Earbuds', 'audio', 99, 4.3, ['headphones', 'wireless'], 'Small wireless earbuds for running and the commute.', { travel: 0.3, outdoors: 0.3 }],
  ['Audiobook subscription', 'audio', 15, 4.2, ['books', 'subscription'], 'A month of audiobooks, a new one each week.', { reading: 0.8 }],
];

const AXIS = { coffee: 0, tea: 1, kitchen: 2, baking: 3, outdoor: 4, travel: 5, books: 6, audio: 7 };

/** A product's vector: its category's axis, the others it names, made of length one. */
function taste(category, also) {
  const v = new Array(AXES.length).fill(0);
  v[AXIS[category]] = 1;
  for (const [axis, w] of Object.entries(also)) v[AXES.indexOf(axis)] += w;
  const n = Math.hypot(...v);
  return v.map((x) => Math.round((x / n) * 1000) / 1000);
}

/** A small generator of its own, so every visit seeds the same rows. */
function random(seed) {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

const CUSTOMERS = ['ada', 'grace', 'linus', 'barbara', 'ken', 'margaret', 'dennis', 'frances', 'guido', 'radia', 'alan', 'hedy', 'edsger', 'karen', 'john', 'sophie', 'tim', 'anita', 'bjarne', 'joan'];
const STATUSES = ['paid', 'shipped', 'delivered', 'delivered', 'delivered', 'refunded'];
const DAY = 86_400_000;
/** The week the events cover, and the month the orders do: fixed, so the examples' dates hold. */
const WEEK = Date.UTC(2026, 8, 28);

function orders(rand) {
  const out = [];
  for (let i = 0; i < 400; i++) {
    const p = Math.floor(rand() ** 1.6 * PRODUCTS.length);
    const qty = 1 + Math.floor(rand() * rand() * 4);
    out.push({
      product: p + 1,
      customer: CUSTOMERS[Math.floor(rand() ** 1.3 * CUSTOMERS.length)],
      qty,
      total: Math.round(PRODUCTS[p][2] * qty * 100) / 100,
      status: STATUSES[Math.floor(rand() * STATUSES.length)],
      placed: new Date(WEEK - 21 * DAY + Math.floor(rand() * 28 * DAY)).toISOString(),
    });
  }
  return out;
}

/** A week of a shop's events: views most, then carts, then purchases; busier by day than by night. */
function events(rand) {
  const out = [];
  const users = 900;
  for (let i = 0; i < 12_000; i++) {
    // A day's hours weighted toward the afternoon and evening.
    const day = Math.floor(rand() * 7);
    const hour = Math.min(23, Math.floor(8 + Math.abs(rand() + rand() + rand() - 1.5) * 9.5));
    const at = WEEK + day * DAY + hour * 3_600_000 + Math.floor(rand() * 3_600_000);
    const r = rand();
    const name = r < 0.7 ? 'view' : r < 0.85 ? 'search' : r < 0.95 ? 'add_to_cart' : 'purchase';
    out.push({
      user: `u${String(Math.floor(rand() ** 2 * users)).padStart(4, '0')}`,
      name,
      product: name === 'search' ? null : 1 + Math.floor(rand() ** 1.6 * PRODUCTS.length),
      at: new Date(at).toISOString(),
    });
  }
  return out.sort((a, b) => (a.at < b.at ? -1 : 1));
}

export function seed() {
  const rand = random(0xfe7ec);
  const products = PRODUCTS.map(([name, category, price, rating, tags, description, also]) => ({
    name,
    category,
    description,
    price,
    rating,
    tags,
    taste: taste(category, also),
  }));
  return [
    'create collection products (name text, category text @hash, description text @text, price float @sorted, rating float, tags [text], taste vector<8> @hnsw(cosine))',
    'create collection orders (product int @hash, customer text @hash, qty int, total float, status text @hash, placed timestamp @sorted)',
    'create collection events (user text @hash, name text @hash, product int, at timestamp @sorted)',
    ['put products $1', [products]],
    ['put orders $1', [orders(rand)]],
    ['put events $1', [events(rand)]],
  ];
}

// --------------------------------------------------------------- examples
//
// What the editor's Examples list holds. Each opens in the editor and runs
// there as typed, so it can be changed and run again.

export const EXAMPLES = [
  {
    name: 'Search by words',
    text: '-- Ranked by BM25 over the @text index\nget products select name, category\n  match description "pour over coffee"\n  limit 10',
  },
  {
    name: 'Search by meaning',
    text: `-- The vector's axes: ${AXES.join(', ')}.\n-- Something for coffee, on the road:\nget products select name, category\n  near taste [0.9, 0, 0, 0, 0, 0.5, 0, 0]\n  limit 5`,
  },
  {
    name: 'Words and meaning, fused',
    text: '-- Each search ranks the products; fuse adds the two ranks\nget products select name, category\n  match description "kettle"\n  near taste [0, 0.8, 0.3, 0, 0, 0, 0, 0]\n  fuse limit 8',
  },
  {
    name: 'A filter, ordered',
    text: 'get products select name, price, rating\n  where category = $1 and price < $2\n  order price desc\n  limit 20',
    params: '["coffee", 100]',
  },
  {
    name: 'Counts by value',
    text: '-- The rows a filter selects, counted by category and by tag\nget products where price < 50\n  limit 0\n  facet category, tags top 6',
  },
  {
    name: 'Orders with their product',
    text: '-- lookup: each order with the product it points at\nget orders select customer, qty, total, placed\n  order placed desc limit 10\n  lookup products on id = product select name, price',
  },
  {
    name: 'Products with orders',
    text: '-- Each audio product with its three latest orders\nget products select name\n  where category = "audio"\n  lookup orders on product order placed desc limit 3',
  },
  {
    name: 'Revenue by status',
    text: 'get orders select status, count(*) as orders, sum(total) as revenue\n  group status\n  order revenue desc',
  },
  {
    name: 'Visitors by day',
    text: '-- approx_count_distinct: a HyperLogLog sketch, 0.8% error\nget events select bucket(at, 1d) as day, count(*) as events,\n    approx_count_distinct(user) as visitors\n  group day\n  order day',
  },
  {
    name: 'Busiest hours',
    text: '-- having keeps the groups that pass\nget events select bucket(at, 1h) as hour, count(*) as events\n  where name = "purchase"\n  group hour having count(*) >= 6\n  order events desc limit 10',
  },
  {
    name: 'The plan of a query',
    text: '-- explain: the path a query took, and which index\nexplain get orders where customer = "ada"\n  and placed >= "2026-09-20"\n  order placed desc limit 20',
  },
  {
    name: 'Write a row',
    text: '-- Then open Rows, or Live on products, to see it\ninsert products {name: "A kettle of my own", category: "tea",\n  description: "Whistles when the water boils.", price: 29.5,\n  taste: [0, 0.9, 0.4, 0, 0, 0, 0, 0]}',
  },
  {
    name: 'Change prices',
    text: '-- A set reads the row it writes: ten percent off coffee\nset products {price: price * 0.9} where category = "coffee"',
  },
  {
    name: 'Collections',
    text: 'collections',
  },
];
