// Drive the real HA frontend in headless Chromium and record HTTP (/, /auth, /api, /.well-known) and WebSocket
// traffic as JSONL. Usage: [STEPS=json] node capture.mjs <ha-url> <out-dir>   (see capture.sh)
import { createRequire } from "node:module";
import { writeFileSync, statSync } from "node:fs";
import { execSync } from "node:child_process";
import { norm as normFor } from "./norm.mjs";
const puppeteer = createRequire(process.env.NODE_PATH + "/")("puppeteer-core");
const [url, out] = process.argv.slice(2);
const norm = (s) => normFor(s, url);
// Run after the dashboard loads, each once the previous one has settled (see settle()): {"call": [domain, service, data]} goes through the
// frontend's own connection, {"sh": "cmd"} runs on the host (e.g. a device going offline),
// {"sub": msg} opens a WS subscription, {"nav": "/path"} opens a frontend page, {"api": [method, path, data]}
// calls the REST API as the frontend does (`hass.callApi`, e.g. the editors' save); an error reply is recorded, not thrown.
const steps = JSON.parse(process.env.STEPS ||'[{"call": ["light", "toggle", {"entity_id": "light.kitchen"}]}]');
// The frontend posts /auth/token as multipart; keep only the fields.
const form = (s) => Object.fromEntries([...s.matchAll(/name="([^"]+)"\r\n\r\n([^\r]*)/g)].map((m) => [m[1], m[2]]));
const parse = (s) => { try { return JSON.parse(norm(s)); } catch { return norm(s); } };

const http = [], ws = [];
// settle(max): wait until nothing has happened for QUIET_MS (default 500) — no HTTP response, WS frame, or new line
// in $MQTT_LOG (the broker log, when MQTT is recorded) — or max ms, whichever comes first. Replaces fixed sleeps;
// a message later than the quiet window lands in the next step and shows up as a diff, never as a silent pass.
const QUIET = Number(process.env.QUIET_MS ?? 500);
let last = Date.now();
const logSize = () => { try { return process.env.MQTT_LOG ? statSync(process.env.MQTT_LOG).size : 0; } catch { return 0; } };
async function settle(max) {
  const start = last = Date.now();
  let size = logSize();
  while (Date.now() - start < max && Date.now() - last < QUIET) {
    await new Promise((r) => setTimeout(r, 50));
    const n = logSize();
    if (n !== size) { size = n; last = Date.now(); }
  }
}
const browser = await puppeteer.launch({ executablePath: process.env.CHROMIUM ?? "/usr/bin/chromium", args: ["--no-sandbox"] });
const page = await browser.newPage();
await page.setViewport({ width: 1280, height: 900 });

page.on("response", async (res) => {
  const req = res.request(), p = new URL(req.url());
  if (p.origin !== new URL(url).origin) return;
  last = Date.now();
  const isDoc = req.resourceType() === "document";
  if (!isDoc && !/^\/(auth|api|\.well-known)\//.test(p.pathname)) return;
  let body = null;
  try { body = isDoc ? "<html>" : parse(await res.text()); } catch {}
  if (isDoc && p.pathname + p.search === "/") writeFileSync(`${out}/index.html`, norm(await res.text()));
  http.push({ method: req.method(), path: norm(p.pathname + p.search), status: res.status(),
    req: req.postData() ? parse(req.postData().includes("form-data;") ? JSON.stringify(form(req.postData())) : req.postData()) : undefined, res: body });
});

// For diagnosing a failed run (CI uploads $out on failure): console output, page errors, failed requests.
const diag = [];
page.on("console", (m) => diag.push(`console.${m.type()}: ${m.text()}`));
page.on("pageerror", (e) => diag.push(`pageerror: ${e.message}`));
page.on("requestfailed", (r) => diag.push(`requestfailed: ${r.url()} ${r.failure()?.errorText}`));

const cdp = await page.createCDPSession();
await cdp.send("Network.enable");
cdp.on("Network.webSocketFrameSent", (e) => { last = Date.now(); ws.push({ dir: "send", msg: parse(e.response.payloadData) }); });
cdp.on("Network.webSocketFrameReceived", (e) => { last = Date.now(); ws.push({ dir: "recv", msg: parse(e.response.payloadData) }); });

try {
  // Chromium aborts loads with ERR_NETWORK_CHANGED when an interface changes, e.g. the broker
  // container's veth still coming up; then the login form never appears. Load it once more, with
  // the recording restarted so no request is counted twice.
  for (let attempt = 1; ; attempt++) {
    await page.goto(url + "/", { waitUntil: "networkidle2" });
    const form = await page.waitForSelector("pierce/input[name=username]", { timeout: 15000 }).catch(() => null);
    if (form || attempt === 2) break;
    diag.push("login form missing, reloading");
    http.length = 0;
    ws.length = 0;
  }
  await page.locator("pierce/input[name=username]").fill("dev");
  await page.locator("pierce/input[name=password]").fill("dev");
  await page.keyboard.press("Enter");
  await page.waitForSelector("pierce/hui-view", { timeout: 60000 });
  await settle(5000);
  for (const step of steps) {
    if (step.sh) execSync(step.sh, { stdio: "inherit" });
    // A subscription (e.g. render_template) that stays open, so later updates are recorded too.
    else if (step.sub) await page.evaluate((m) => { document.querySelector("home-assistant").hass.connection.subscribeMessage(() => {}, m); }, step.sub);
    // In-app navigation, as the frontend's navigate() does it; a reload would log out.
    else if (step.nav) await page.evaluate((p) => { history.pushState(null, "", p); window.dispatchEvent(new CustomEvent("location-changed")); }, step.nav);
    else if (step.api) await page.evaluate((a) => document.querySelector("home-assistant").hass.callApi(...a).catch(() => null), step.api);
    else await page.evaluate((c) => document.querySelector("home-assistant").hass.callService(...c), step.call);
    await settle(3000);
  }
} catch (e) {
  diag.push(`failed at ${page.url()}: ${e.message}`);
  writeFileSync(`${out}/failure.log`, diag.join("\n") + "\n");
  await page.screenshot({ path: `${out}/failure.png` }).catch(() => {});
  throw e;
} finally {
  await browser.close();
}

const jsonl = (a) => a.map((o) => JSON.stringify(o)).join("\n") + "\n";
writeFileSync(`${out}/http.jsonl`, jsonl(http));
writeFileSync(`${out}/ws.jsonl`, jsonl(ws));
console.log(`http=${http.length} ws=${ws.length}`);
