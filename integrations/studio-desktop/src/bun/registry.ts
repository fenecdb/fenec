// userData/servers.json: every server this app starts is noted here (strays.ts).

import { join } from "node:path";
import { Utils } from "electrobun/main";
import { Strays } from "./strays.ts";

export const strays = new Strays(join(Utils.paths.userData, "servers.json"));
