// Volatile values become placeholders so re-runs diff cleanly. Used by capture.mjs, and as a CLI
// for files captured outside the browser: node norm.mjs <ha-url> <in> <out>
import { readFileSync, writeFileSync } from "node:fs";
import { argv } from "node:process";
import { fileURLToPath } from "node:url";

export const norm = (s, url) =>
  s
    .replaceAll(url, "http://HA")
    .replaceAll(encodeURIComponent(url), encodeURIComponent("http://HA"))
    .replace(/eyJ[\w-]+\.[\w-]+\.[\w-]+/g, "<jwt>")
    .replace(/"(access_token|refresh_token|auth_code|flow_id|code_challenge|code_verifier|token)":"[^"]+"/g, '"$1":"<$1>"')
    .replace(/([?&](code|state|code_challenge)=)[^&"]+/g, "$1<$2>")
    .replace(/\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(\.\d+)?(\+00:00|Z)?/g, "<ts>")
    .replace(/\b1[6-9]\d{8}(\.\d+)?\b/g, "0") // epoch seconds, e.g. "lc", "created_at"
    .replace(/\b[0-9A-HJKMNP-TV-Z]{26}\b/g, "<ulid>")
    .replace(/\b[0-9a-f]{32}\b/g, "<hex32>");

if (argv[1] === fileURLToPath(import.meta.url)) {
  const [url, input, output] = argv.slice(2);
  writeFileSync(output, norm(readFileSync(input, "utf8"), url));
}
