// Cloudflare serves static files from a plain directory, so the viewer is
// copied out of ../viewer before every dev/deploy run. viewer/ stays the
// single source of truth.

import { cpSync, mkdirSync, readdirSync, rmSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const source = join(here, "..", "viewer");
const target = join(here, "public");

rmSync(target, { recursive: true, force: true });
mkdirSync(target, { recursive: true });
cpSync(source, target, { recursive: true });

console.log(`viewer -> cloudflare/public (${readdirSync(target).join(", ")})`);
