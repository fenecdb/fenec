// What the generator (lib/generate.ts) draws a product from: thirty
// categories of desert and travel gear, each with the words its products
// are named and described in, and the brands that make them. All invented:
// no brand here is a real company's.

export interface CategoryDef {
  slug: string;
  name: string;
  department: string;
  blurb: string;
  /** The silhouette its products' pictures draw (lib/shapes.ts). */
  shape: string;
  /** What one product is called, after brand and model: "2-person tent". */
  nouns: string[];
  sizes: string[];
  materials: string[];
  weight: [number, number]; // grams
  price: [number, number]; // cents
  /** First sentences, with {model}, {material}, {colour}, {size}. */
  openers: string[];
  /** Second sentences: what it is good for. */
  uses: string[];
}

export const COLOURS = [
  'sand', 'indigo', 'saffron', 'slate', 'olive', 'rust', 'bone', 'charcoal', 'oasis green', 'dusk blue', 'clay', 'sage',
];

export const BRANDS = [
  'Harmattan', 'Qanat', 'Sabkha', 'Barchan', 'Tassili', 'Simoom', 'Ksar', 'Wadi & Co', 'Hoggar', 'Khamsin',
  'Ténéré Works', 'Draa', 'Zerzura', 'Reg Supply', 'Atlas Field', 'Mesa Line', 'Caravan Goods', 'Saltpan',
  'Nomad Thread', 'Dunebuilt', 'Mirage Labs', 'Oasis Kit', 'Fesh Fesh', 'Erg Outfitters', 'Gibber', 'Seif',
  'Sandgrouse', 'Playa', 'Bajada', 'Yardang', 'Arroyo Supply', 'Lee Wall',
];

/** Model names: landforms and winds of dry country. */
export const MODELS = [
  'Slipface', 'Crest', 'Ripple', 'Star', 'Seif', 'Barchan', 'Yardang', 'Playa', 'Hamada', 'Mesa', 'Butte',
  'Wadi', 'Oasis', 'Zephyr', 'Leeward', 'Ventifact', 'Bajada', 'Arroyo', 'Caliche', 'Pediment', 'Gibber',
  'Inselberg', 'Fulgurite', 'Sirocco', 'Khamsin', 'Ghibli', 'Haboob', 'Mirage', 'Erg', 'Reg', 'Sabkha',
  'Draa', 'Qasr', 'Nomad', 'Caravan', 'Saltpan', 'Dustline', 'Dune Sea', 'Hardpan', 'Mesquite',
];

export const CATEGORIES: CategoryDef[] = [
  // ---------------------------------------------------------------- Shelter and sleep
  {
    slug: 'tents', name: 'Tents', department: 'Shelter and sleep', shape: 'tent',
    blurb: 'Shelters that stay put in a crosswind and shed blown sand instead of trapping it.',
    nouns: ['backpacking tent', 'ultralight tent', 'tunnel tent', 'dome tent', 'trekking-pole tent', 'basecamp tent'],
    sizes: ['1-person', '2-person', '3-person', '4-person'],
    materials: ['silnylon', 'polyester ripstop', 'Dyneema composite', 'cotton canvas'],
    weight: [780, 4200], price: [12900, 64900],
    openers: [
      'The {model} pitches in under five minutes with a single hubbed pole, and its {material} fly sheds blown sand instead of holding it.',
      'A low, wind-shaped profile keeps the {model} quiet in a gusting night, and the {colour} {material} fly stays cool under a hard sun.',
      'The {model} is a {size} shelter built around a sand-skirt that you bury, so the wind has nothing to lift.',
    ],
    uses: [
      'Two doors and two vestibules mean nobody climbs over anyone for a dawn start.',
      'Mesh panels run the length of the inner, so the night air moves through instead of pooling.',
      'Guy-out points are reinforced for deadman anchors, which hold where stakes will not.',
    ],
  },
  {
    slug: 'sleeping-bags', name: 'Sleeping bags', department: 'Shelter and sleep', shape: 'bag',
    blurb: 'Warm enough for a clear desert night, which is colder than most people pack for.',
    nouns: ['sleeping bag', 'mummy bag', 'down quilt', 'summer bag'],
    sizes: ['regular', 'long', 'wide'],
    materials: ['800-fill down', '650-fill down', 'synthetic fill', 'recycled synthetic fill'],
    weight: [420, 1600], price: [8900, 42900],
    openers: [
      'Desert nights drop fast after sunset, and the {model} keeps you warm to about 2 °C with {material} in baffles that do not shift.',
      'The {model} packs down to the size of a water bottle, and its {material} lofts back within minutes of unrolling.',
      'A {size} cut in the {model} leaves room to turn over without letting the cold in at the shoulders.',
    ],
    uses: [
      'The full-length zip opens it flat as a quilt for the warm nights either side.',
      'A draft collar and a cinchable hood seal in heat when the wind finds the tent.',
      'The shell is treated to shrug off dew and the condensation of a cold dawn.',
    ],
  },
  {
    slug: 'sleeping-pads', name: 'Sleeping pads', department: 'Shelter and sleep', shape: 'pad',
    blurb: 'Insulation from ground that holds the day’s heat until midnight and then gives it all away.',
    nouns: ['sleeping pad', 'air mat', 'foam pad', 'self-inflating mat'],
    sizes: ['short', 'regular', 'long', 'wide'],
    materials: ['TPU-laminated nylon', 'closed-cell foam', 'open-cell foam', 'recycled polyester'],
    weight: [290, 1300], price: [3900, 21900],
    openers: [
      'The {model} puts a reflective layer between you and the ground, which matters more on sand than most people expect.',
      'Thorn-strewn ground is no trouble for the {model}: its {material} shrugs off acacia spines.',
      'A {size} {model} inflates in a dozen breaths through a one-way valve that does not leak back.',
    ],
    uses: [
      'Horizontal baffles stop you rolling off in the night.',
      'It rolls up small enough to strap under a pack lid.',
      'A repair kit rides in the stuff sack, because nobody carries one separately.',
    ],
  },
  {
    slug: 'shade-shelters', name: 'Shade and tarps', department: 'Shelter and sleep', shape: 'tarp',
    blurb: 'Midday shade is the most valuable thing you can carry in dry country.',
    nouns: ['tarp', 'sun shade', 'shade sail', 'bivy tarp'],
    sizes: ['2 × 3 m', '3 × 3 m', '3 × 4 m', '4 × 5 m'],
    materials: ['silver-coated polyester', 'silnylon', 'canvas', 'knitted HDPE shade cloth'],
    weight: [310, 2600], price: [3500, 18900],
    openers: [
      'The {model} throws a {size} pool of shade and lets the breeze through underneath, which a tent cannot.',
      'Its reflective {material} face turns away most of the sun, and the {colour} underside is easy on the eyes.',
      'Pitch the {model} high at noon and low at dusk: sixteen tie-outs give you every shape in between.',
    ],
    uses: [
      'Ridge loops take a trekking pole or a vehicle roof rail.',
      'It packs into its own corner pocket, so the bag is never lost.',
      'Rolled hems keep the edges from fraying in a long wind.',
    ],
  },
  // ---------------------------------------------------------------- Carry
  {
    slug: 'backpacks', name: 'Backpacks', department: 'Carry', shape: 'pack',
    blurb: 'Packs that carry water weight on the hips, where it belongs.',
    nouns: ['trekking pack', 'daypack', 'expedition pack', 'travel pack'],
    sizes: ['20 l', '30 l', '45 l', '60 l', '75 l'],
    materials: ['ripstop nylon', 'waxed canvas', 'recycled polyester', 'ultra-high-molecular-weight polyethylene'],
    weight: [520, 2400], price: [5900, 34900],
    openers: [
      'The {model} is a {size} pack with a suspended mesh back, so air moves between you and the load on a hot climb.',
      'Water is the heaviest thing you carry, and the {model} puts it low and close with two sleeves inside the back panel.',
      'Built from {material}, the {model} takes a season of abrasion from rock and scrub and still looks like itself.',
    ],
    uses: [
      'The hip belt has pockets big enough for a phone and a map.',
      'A sand-proof roll top keeps the main body sealed in a storm.',
      'Side pockets take a litre bottle you can reach without stopping.',
    ],
  },
  {
    slug: 'duffels', name: 'Duffels', department: 'Carry', shape: 'duffel',
    blurb: 'Bags that ride a roof rack, a camel and an overhead bin without complaint.',
    nouns: ['duffel', 'expedition duffel', 'rolling duffel', 'gear hauler'],
    sizes: ['40 l', '60 l', '90 l', '120 l'],
    materials: ['TPU-coated polyester', 'waxed canvas', 'ballistic nylon', 'recycled tarpaulin'],
    weight: [900, 3200], price: [6900, 27900],
    openers: [
      'The {model} is a {size} duffel in {material} that you can drag across gravel and hose down afterwards.',
      'A wide D-zip opening means the {model} packs like a suitcase and carries like a bag.',
      'Shoulder straps stow in a back pocket, so the {model} goes from roof rack to back in a minute.',
    ],
    uses: [
      'Lash points at every corner hold it down on a moving vehicle.',
      'An end pocket keeps damp or dusty kit away from the rest.',
      'The zips are big, gloved-hand teeth that do not jam with sand.',
    ],
  },
  {
    slug: 'hydration-packs', name: 'Hydration packs', department: 'Carry', shape: 'hydration',
    blurb: 'Drink without stopping, which in the heat means drinking enough.',
    nouns: ['hydration pack', 'hydration vest', 'running vest', 'bike hydration pack'],
    sizes: ['1.5 l', '2 l', '3 l', '4 l'],
    materials: ['ripstop nylon', 'stretch mesh', 'recycled polyester', 'TPU'],
    weight: [180, 820], price: [3900, 16900],
    openers: [
      'The {model} carries a {size} reservoir against your back with an insulated tube, so the first sip is not hot.',
      'A vest cut keeps the {model} still on a fast walk, with soft flasks on the chest within reach.',
      'Its {material} body breathes, and the {colour} panels show up against brown hills.',
    ],
    uses: [
      'The reservoir opens wide for ice and cleaning.',
      'A magnetic clip holds the bite valve where your hand expects it.',
      'There is room for a shell, a snack and a headlamp, and not much else, which is the point.',
    ],
  },
  {
    slug: 'packing-cubes', name: 'Packing cubes', department: 'Carry', shape: 'cube',
    blurb: 'Order inside the bag, so a dusty camp does not get into everything.',
    nouns: ['packing cube set', 'compression cube', 'dry bag set', 'shoe bag'],
    sizes: ['small', 'medium', 'large', 'set of three'],
    materials: ['ripstop nylon', 'silnylon', 'recycled polyester', 'TPU-coated nylon'],
    weight: [40, 380], price: [1500, 6900],
    openers: [
      'The {model} keeps clothes in rolls and sand outside, in {material} that weighs almost nothing.',
      'A compression zip on the {model} takes a third off the volume of a week’s clothes.',
      'Colour-code a trip: the {colour} {model} is for the clean things.',
    ],
    uses: [
      'Mesh tops let you see what is inside without opening it.',
      'They nest into each other when the bag is empty.',
      'Grab loops at each end make pulling one from a full pack easy.',
    ],
  },
  // ---------------------------------------------------------------- Water and kitchen
  {
    slug: 'water-bottles', name: 'Water bottles', department: 'Water and kitchen', shape: 'bottle',
    blurb: 'Bottles that keep water cool through an afternoon in the sun.',
    nouns: ['insulated bottle', 'water bottle', 'wide-mouth bottle', 'flask'],
    sizes: ['500 ml', '750 ml', '1 l', '1.5 l'],
    materials: ['double-wall stainless steel', 'titanium', 'Tritan copolyester', 'aluminium'],
    weight: [90, 640], price: [1900, 6900],
    openers: [
      'The {model} holds {size} and keeps it cold for a full day in a hot car, which is the real test.',
      'Made of {material}, the {model} takes a fall onto rock and keeps its seal.',
      'A wide mouth on the {model} takes ice cubes and a filter, and a {colour} powder coat that does not chip.',
    ],
    uses: [
      'The cap has a loop that fits two fingers or a carabiner.',
      'It fits a car cup holder and a pack side pocket.',
      'Nothing in it holds a taste from yesterday’s coffee.',
    ],
  },
  {
    slug: 'water-filters', name: 'Water filters', department: 'Water and kitchen', shape: 'filter',
    blurb: 'Make a well, a gelta or a cattle trough drinkable.',
    nouns: ['water filter', 'gravity filter', 'filter bottle', 'purifier'],
    sizes: ['0.5 l', '1 l', '4 l', '10 l'],
    materials: ['hollow fibre', 'ceramic', 'activated carbon', 'UV lamp'],
    weight: [60, 520], price: [2900, 14900],
    openers: [
      'The {model} filters {size} in a few minutes through {material}, removing bacteria and protozoa.',
      'Hang the {model} from a branch and let gravity do the work while you set up camp.',
      'A backflush syringe keeps the {model} running on silty water that would choke most filters.',
    ],
    uses: [
      'It threads onto most bottles and soft flasks.',
      'Each cartridge is rated for thousands of litres.',
      'It works in the cold without a battery to fail.',
    ],
  },
  {
    slug: 'stoves', name: 'Stoves', department: 'Water and kitchen', shape: 'stove',
    blurb: 'Boil water in a wind that would blow out a match.',
    nouns: ['canister stove', 'multi-fuel stove', 'wood stove', 'alcohol stove'],
    sizes: ['solo', 'two-person', 'group'],
    materials: ['titanium', 'stainless steel', 'aluminium', 'brass'],
    weight: [45, 900], price: [2900, 18900],
    openers: [
      'The {model} boils a litre in about four minutes, and its built-in windscreen keeps doing so in a stiff breeze.',
      'Burning wood, the {model} needs no fuel you have to carry, just the dry twigs desert camps are full of.',
      'A piezo lighter and a wide {material} pot support make the {model} simple to use in the dark.',
    ],
    uses: [
      'It folds into its own pot.',
      'The simmer control is fine enough for rice, not just boiling.',
      'Its feet are wide enough to stand on sand without sinking.',
    ],
  },
  {
    slug: 'cookware', name: 'Cookware', department: 'Water and kitchen', shape: 'pot',
    blurb: 'Pots and pans that nest together and clean up with sand and a little water.',
    nouns: ['cook pot', 'pot set', 'frying pan', 'kettle'],
    sizes: ['750 ml', '1.3 l', '2 l', '3-piece set'],
    materials: ['hard-anodised aluminium', 'titanium', 'stainless steel', 'cast iron'],
    weight: [110, 1900], price: [1900, 11900],
    openers: [
      'The {model} is {material}, which heats evenly over a small flame and does not dent when dropped.',
      'Graduations stamped inside the {model} mean you can measure water without a cup.',
      'A {size} {model} with folding handles that lock, so the pot does not tip off the flame.',
    ],
    uses: [
      'A strainer lid pours pasta water away without losing the pasta.',
      'It nests a 230 g gas canister inside.',
      'The non-stick inside wipes clean with a handful of sand.',
    ],
  },
  {
    slug: 'coolers', name: 'Coolers', department: 'Water and kitchen', shape: 'cooler',
    blurb: 'Ice that lasts days, in a place where shade is a luxury.',
    nouns: ['hard cooler', 'soft cooler', 'cooler bag', 'ice chest'],
    sizes: ['15 l', '25 l', '45 l', '65 l'],
    materials: ['rotomoulded polyethylene', 'TPU-coated nylon', 'closed-cell foam', 'stainless steel'],
    weight: [800, 12000], price: [4900, 39900],
    openers: [
      'The {model} keeps ice for up to five days in the shade of a vehicle, with walls of thick {material}.',
      'A gasketed lid on the {model} seals out dust as well as warm air.',
      'The {colour} {model} reflects more sun than a dark cooler and keeps its ice longer for it.',
    ],
    uses: [
      'Tie-down slots fit standard roof rack straps.',
      'The drain plug is leakproof and big enough to empty fast.',
      'It is rated as a seat, which saves carrying a chair.',
    ],
  },
  // ---------------------------------------------------------------- Light and power
  {
    slug: 'headlamps', name: 'Headlamps', department: 'Light and power', shape: 'headlamp',
    blurb: 'Hands-free light for pitching camp after the sun goes, which it does quickly.',
    nouns: ['headlamp', 'rechargeable headlamp', 'running headlamp', 'red-light headlamp'],
    sizes: ['200 lm', '400 lm', '700 lm', '1000 lm'],
    materials: ['polycarbonate', 'aluminium', 'ABS', 'silicone'],
    weight: [38, 180], price: [1900, 9900],
    openers: [
      'The {model} throws {size} far enough to read a track, and dims to a red light that keeps your night vision.',
      'A lockout switch stops the {model} from turning itself on in a pack and draining overnight.',
      'Charge the {model} by USB-C from the same bank as your phone.',
    ],
    uses: [
      'It is rated to keep out wind-blown dust and a downpour.',
      'The strap is wide and soft, comfortable over a hat.',
      'A battery gauge shows how many hours are left, not just a light.',
    ],
  },
  {
    slug: 'lanterns', name: 'Lanterns', department: 'Light and power', shape: 'lantern',
    blurb: 'Light a whole camp with something that weighs less than a can of beans.',
    nouns: ['camp lantern', 'collapsible lantern', 'string lights', 'solar lantern'],
    sizes: ['150 lm', '300 lm', '500 lm'],
    materials: ['silicone', 'aluminium', 'polycarbonate', 'recycled plastic'],
    weight: [90, 640], price: [1900, 7900],
    openers: [
      'The {model} spreads a soft {size} glow across a camp without dazzling anyone.',
      'Collapsed, the {model} is flat enough for a side pocket; opened, it lights a tent wall to wall.',
      'A solar panel on top of the {model} charges it while it rides on your pack.',
    ],
    uses: [
      'A hook on the base hangs it upside down from a tent loop.',
      'Its warm setting keeps insects from crowding it.',
      'It doubles as a power bank for a phone in a pinch.',
    ],
  },
  {
    slug: 'solar-chargers', name: 'Solar chargers', department: 'Light and power', shape: 'solar',
    blurb: 'More sun than you will ever need, turned into a full phone.',
    nouns: ['solar panel', 'folding solar charger', 'solar power bank', 'solar kit'],
    sizes: ['10 W', '21 W', '40 W', '100 W'],
    materials: ['monocrystalline silicon', 'ETFE-laminated cells', 'PET-laminated cells'],
    weight: [220, 4800], price: [3900, 29900],
    openers: [
      'The {model} folds out to {size} of {material} and charges a phone about as fast as a wall socket on a clear day.',
      'Sand and grit wipe off the {model}’s laminated face without scratching it.',
      'Hang the {model} off the back of a pack and arrive with a full battery.',
    ],
    uses: [
      'It restarts charging by itself after a cloud passes.',
      'Two USB ports and a DC output cover most things you would bring.',
      'Corner grommets let you stake it flat in a wind.',
    ],
  },
  {
    slug: 'power-banks', name: 'Power banks', department: 'Light and power', shape: 'battery',
    blurb: 'Days of charge for the phone you navigate with.',
    nouns: ['power bank', 'rugged power bank', 'portable power station', 'battery pack'],
    sizes: ['10 000 mAh', '20 000 mAh', '26 800 mAh', '300 Wh'],
    materials: ['aluminium', 'rubberised polycarbonate', 'ABS'],
    weight: [180, 3600], price: [2900, 29900],
    openers: [
      'The {model} holds {size}, about five full phone charges, in a case that takes a drop on rock.',
      'Charge the {model} from a solar panel during the day and your devices from it at night.',
      'A sealed {material} case keeps fine dust out of the ports of the {model}.',
    ],
    uses: [
      'Pass-through charging lets it top up a phone while it charges itself.',
      'A small torch on the end is surprisingly useful.',
      'It is under the limit most airlines allow in the cabin.',
    ],
  },
  // ---------------------------------------------------------------- Wear
  {
    slug: 'sun-hats', name: 'Sun hats', department: 'Wear', shape: 'hat',
    blurb: 'Shade for your face and neck that stays on in a gust.',
    nouns: ['sun hat', 'wide-brim hat', 'legionnaire cap', 'bucket hat'],
    sizes: ['S/M', 'L/XL', 'one size'],
    materials: ['nylon with UPF 50', 'cotton canvas', 'straw', 'linen'],
    weight: [60, 180], price: [1900, 7900],
    openers: [
      'The {model} has a wide brim and a neck cape in {material}, which blocks the sun where sunscreen wears off.',
      'A chin cord with a toggle keeps the {model} on in a wind that would take most hats.',
      'Mesh vents in the crown of the {model} let heat out the top.',
    ],
    uses: [
      'It crushes into a pocket and springs back.',
      'The sweatband wicks and dries fast.',
      'It floats, which you will appreciate at an oasis pool.',
    ],
  },
  {
    slug: 'scarves', name: 'Scarves and shemaghs', department: 'Wear', shape: 'scarf',
    blurb: 'Wrap your face against sand, sun and the cold at dawn.',
    nouns: ['shemagh', 'tagelmust', 'neck gaiter', 'travel scarf'],
    sizes: ['110 × 110 cm', '120 × 120 cm', '5 m', 'one size'],
    materials: ['cotton', 'linen', 'merino', 'cotton voile'],
    weight: [50, 260], price: [1500, 5900],
    openers: [
      'The {model} is a {size} square of {material}, wide enough to wrap head and face against a sandstorm.',
      'Wet the {model} and wear it on your neck: the evaporation cools you more than you would think.',
      'Woven in {colour} {material}, the {model} softens with every wash.',
    ],
    uses: [
      'It doubles as a towel, a pot holder and a sling.',
      'The weave is open enough to breathe through and close enough to stop grit.',
      'It packs into nothing.',
    ],
  },
  {
    slug: 'sunglasses', name: 'Sunglasses', department: 'Wear', shape: 'glasses',
    blurb: 'Glare off sand and salt is harder on the eyes than snow.',
    nouns: ['sunglasses', 'glacier glasses', 'sport sunglasses', 'polarised sunglasses'],
    sizes: ['narrow fit', 'medium fit', 'wide fit'],
    materials: ['polarised polycarbonate lenses', 'glass lenses', 'photochromic lenses', 'nylon frames'],
    weight: [22, 48], price: [3900, 18900],
    openers: [
      'The {model} has {material} that cut the glare off a salt pan to something you can look across.',
      'Side shields on the {model} keep blown grit out of your eyes.',
      'A {size} frame keeps the {model} close to your face so light does not creep in under the lenses.',
    ],
    uses: [
      'The lenses are rated category 4 for the brightest light.',
      'Rubber nose pads grip even when you are sweating.',
      'A hard case with a carabiner loop is included.',
    ],
  },
  {
    slug: 'shirts', name: 'Sun shirts', department: 'Wear', shape: 'shirt',
    blurb: 'Long sleeves that keep you cooler than bare skin under a hard sun.',
    nouns: ['sun shirt', 'button-down shirt', 'hooded sun shirt', 'base layer'],
    sizes: ['XS', 'S', 'M', 'L', 'XL', 'XXL'],
    materials: ['nylon with UPF 50', 'linen', 'merino', 'recycled polyester'],
    weight: [140, 320], price: [3900, 11900],
    openers: [
      'The {model} is cut loose in {material}, so air moves under it and the sun stays off your arms.',
      'Vents across the back of the {model} open up when you are moving and close when you stop.',
      'A sun hood on the {model} covers the back of your neck without a hat cape.',
    ],
    uses: [
      'It dries in an hour on a guy line.',
      'Roll-up tabs hold the sleeves in place when you want them up.',
      'It does not hold a smell after a long day.',
    ],
  },
  {
    slug: 'trousers', name: 'Trousers', department: 'Wear', shape: 'trousers',
    blurb: 'Trousers that keep thorns and the sun off your legs and still let you climb.',
    nouns: ['trekking trousers', 'convertible trousers', 'travel chinos', 'cargo trousers'],
    sizes: ['28', '30', '32', '34', '36', '38'],
    materials: ['stretch nylon', 'cotton canvas', 'ripstop polyester', 'linen blend'],
    weight: [220, 520], price: [4900, 14900],
    openers: [
      'The {model} is {material} with a gusseted crotch, so stepping up onto a rock ledge is easy.',
      'Zip-off legs turn the {model} into shorts at the oasis.',
      'A {colour} {model} hides dust better than anything else you will pack.',
    ],
    uses: [
      'A zipped thigh pocket keeps a passport where you can feel it.',
      'The hems cinch over a boot to stop sand getting in.',
      'It sheds a light shower and dries quickly.',
    ],
  },
  {
    slug: 'boots', name: 'Boots', department: 'Wear', shape: 'boot',
    blurb: 'Boots that breathe in the heat and grip on loose rock.',
    nouns: ['desert boot', 'hiking boot', 'trail shoe', 'approach shoe'],
    sizes: ['EU 38', 'EU 40', 'EU 42', 'EU 44', 'EU 46'],
    materials: ['suede and mesh', 'full-grain leather', 'synthetic mesh', 'nubuck'],
    weight: [640, 1500], price: [8900, 26900],
    openers: [
      'The {model} has a {material} upper that lets heat out and a gaiter collar that keeps sand from getting in.',
      'A deep-lugged sole on the {model} bites into scree and loose gravel.',
      'There is no waterproof membrane in the {model}, on purpose: in the desert you need breathing more than sealing.',
    ],
    uses: [
      'A cushioned midsole takes the sting out of a long day on hardpan.',
      'The toe cap is rubber, for kicking steps and stubbing rocks.',
      'They break in within a few days.',
    ],
  },
  {
    slug: 'sandals', name: 'Sandals', department: 'Wear', shape: 'sandal',
    blurb: 'For camp, for oasis pools and, on easy ground, for walking.',
    nouns: ['trail sandal', 'camp sandal', 'water sandal', 'slide'],
    sizes: ['EU 38', 'EU 40', 'EU 42', 'EU 44', 'EU 46'],
    materials: ['polyester webbing', 'leather', 'recycled rubber', 'cork'],
    weight: [280, 760], price: [3900, 11900],
    openers: [
      'The {model} straps on with {material} that does not rub when wet or gritty.',
      'A grippy sole on the {model} holds on wet rock at an oasis.',
      'Light enough to clip to a pack, the {model} is what you change into when the boots come off.',
    ],
    uses: [
      'The footbed is contoured and holds its shape.',
      'Adjust all three straps without taking them off.',
      'Rinse them in water and they are clean.',
    ],
  },
  {
    slug: 'socks', name: 'Socks and gaiters', department: 'Wear', shape: 'sock',
    blurb: 'Blisters end trips. Good socks and sand gaiters prevent them.',
    nouns: ['hiking socks', 'liner socks', 'sand gaiters', 'ankle gaiters'],
    sizes: ['S', 'M', 'L', 'XL'],
    materials: ['merino', 'merino blend', 'nylon', 'stretch Lycra'],
    weight: [40, 160], price: [1500, 4900],
    openers: [
      'The {model} is knitted from {material} with padding where boots rub and none where they do not.',
      'Sand gaiters like the {model} close the gap between boot and trouser, which is where sand gets in.',
      'The {model} keeps feet drier than cotton in heat and warmer at night.',
    ],
    uses: [
      'A seamless toe means nothing presses on your toes.',
      'They hold their shape through a week of wear.',
      'Wash them in a stream; they dry overnight.',
    ],
  },
  {
    slug: 'rain-shells', name: 'Shells and windproofs', department: 'Wear', shape: 'jacket',
    blurb: 'For the desert storm that comes once a season and the wind that comes every night.',
    nouns: ['wind shell', 'rain jacket', 'softshell', 'insulated jacket'],
    sizes: ['XS', 'S', 'M', 'L', 'XL'],
    materials: ['2.5-layer nylon', '3-layer polyester', 'softshell', 'synthetic insulation'],
    weight: [90, 640], price: [6900, 29900],
    openers: [
      'The {model} is a {material} shell that stops a dawn wind cold and packs into its own chest pocket.',
      'Pit zips on the {model} let heat out on a climb without letting the wind in.',
      'The {model} takes the evening edge off when the temperature falls twenty degrees in an hour.',
    ],
    uses: [
      'The hood fits over a sun hat.',
      'Its hem and cuffs seal against blown sand.',
      'It weighs about as much as an apple.',
    ],
  },
  // ---------------------------------------------------------------- Navigation and safety
  {
    slug: 'navigation', name: 'Maps and compasses', department: 'Navigation and safety', shape: 'compass',
    blurb: 'For when the track disappears under a dune or the phone runs out.',
    nouns: ['baseplate compass', 'sighting compass', 'GPS unit', 'map case'],
    sizes: ['pocket', 'standard', 'large'],
    materials: ['acrylic', 'aluminium', 'TPU', 'brass'],
    weight: [30, 230], price: [1900, 34900],
    openers: [
      'The {model} has a liquid-damped needle that settles quickly and a declination adjustment you set once.',
      'A sighting mirror on the {model} takes bearings on distant landmarks to a degree.',
      'Satellite messaging on the {model} reaches help where no phone network does.',
    ],
    uses: [
      'Luminous markings work after dark.',
      'A lanyard keeps it from falling into sand.',
      'It needs no battery, so it does not fail when everything else has.',
    ],
  },
  {
    slug: 'first-aid', name: 'First aid', department: 'Navigation and safety', shape: 'aid',
    blurb: 'Kits put together for heat, sun, thorns and long distances from help.',
    nouns: ['first aid kit', 'blister kit', 'trauma kit', 'sun care kit'],
    sizes: ['day', 'weekend', 'expedition'],
    materials: ['ripstop nylon pouch', 'TPU dry pouch', 'hard case'],
    weight: [80, 1200], price: [1500, 12900],
    openers: [
      'The {model} is stocked for heat: electrolytes, blister care and burn gel come before bandages.',
      'Every item in the {model} has its own labelled pocket, so you find it with shaking hands.',
      'A {size} {model} carries enough for a group and a guide to using it.',
    ],
    uses: [
      'Fine-tipped tweezers take out acacia thorns.',
      'An emergency blanket keeps someone warm while waiting.',
      'Everything is replaceable from any pharmacy.',
    ],
  },
  {
    slug: 'knives-tools', name: 'Knives and tools', department: 'Navigation and safety', shape: 'knife',
    blurb: 'The small tool that fixes the thing that broke.',
    nouns: ['folding knife', 'multitool', 'fixed-blade knife', 'trowel'],
    sizes: ['compact', 'mid-size', 'full-size'],
    materials: ['stainless steel', 'titanium', 'carbon steel', 'G10'],
    weight: [40, 320], price: [1900, 14900],
    openers: [
      'The {model} carries a locking blade, pliers and a file in a {material} body that fits a pocket.',
      'A one-hand opening on the {model} works when the other hand is holding the guy line.',
      'The {model} has a blade of {material} that holds an edge through a season of rope and cord.',
    ],
    uses: [
      'Every tool locks open.',
      'A pocket clip keeps it to hand.',
      'It is legal to carry on most of the routes we sell for.',
    ],
  },
  // ---------------------------------------------------------------- Camp
  {
    slug: 'camp-chairs', name: 'Camp chairs and tables', department: 'Camp furniture', shape: 'chair',
    blurb: 'Sit off the sand, out of the ants and up where the breeze is.',
    nouns: ['camp chair', 'low chair', 'folding table', 'stool'],
    sizes: ['compact', 'standard', 'large'],
    materials: ['aluminium and ripstop', 'steel and canvas', 'bamboo', 'aluminium'],
    weight: [450, 4200], price: [2900, 17900],
    openers: [
      'The {model} sets up in a minute on a {material} frame and puts your seat above the heat of the sand.',
      'Wide sand feet on the {model} stop it sinking on soft ground.',
      'Folded, the {model} is the size of a water bottle.',
    ],
    uses: [
      'It holds up to 145 kg.',
      'A side pocket keeps a cup out of the sand.',
      'The seat is mesh where your back meets it.',
    ],
  },
];
