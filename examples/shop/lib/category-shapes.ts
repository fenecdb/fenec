// Each category's silhouette, apart from lib/catalog-data.ts so a card drawn
// in the browser does not bring the whole catalog's copy with it.
// test/seo.test.ts holds the two to the same shapes.
export const CATEGORY_SHAPES: Record<string, string> = {
  tents: 'tent', 'sleeping-bags': 'bag', 'sleeping-pads': 'pad', 'shade-shelters': 'tarp',
  backpacks: 'pack', duffels: 'duffel', 'hydration-packs': 'hydration', 'packing-cubes': 'cube',
  'water-bottles': 'bottle', 'water-filters': 'filter', stoves: 'stove', cookware: 'pot', coolers: 'cooler',
  headlamps: 'headlamp', lanterns: 'lantern', 'solar-chargers': 'solar', 'power-banks': 'battery',
  'sun-hats': 'hat', scarves: 'scarf', sunglasses: 'glasses', shirts: 'shirt', trousers: 'trousers',
  boots: 'boot', sandals: 'sandal', socks: 'sock', 'rain-shells': 'jacket',
  navigation: 'compass', 'first-aid': 'aid', 'knives-tools': 'knife', 'camp-chairs': 'chair',
};
