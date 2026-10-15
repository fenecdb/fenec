// The global entry (entry.js): one URL for every tenant, each request sent
// to the routers of the region the tenant lives in. A plain Worker, not one
// of Workers for Platforms' dispatch namespaces, whose dispatch would sit
// in every database request's path.

import { entry } from './entry.js';

export default entry();
