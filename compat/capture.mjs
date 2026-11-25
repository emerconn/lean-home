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
// {"sub": msg} opens a WS subscription, {"nav": "/path"} opens a frontend page.
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

const cdp = await page.createCDPSession();
await cdp.send("Network.enable");
cdp.on("Network.webSocketFrameSent", (e) => { last = Date.now(); ws.push({ dir: "send", msg: parse(e.response.payloadData) }); });
cdp.on("Network.webSocketFrameReceived", (e) => { last = Date.now(); ws.push({ dir: "recv", msg: parse(e.response.payloadData) }); });

await page.goto(url + "/", { waitUntil: "networkidle2" });
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
  else await page.evaluate((c) => document.querySelector("home-assistant").hass.callService(...c), step.call);
  await settle(3000);
}
await browser.close();

const jsonl = (a) => a.map((o) => JSON.stringify(o)).join("\n") + "\n";
writeFileSync(`${out}/http.jsonl`, jsonl(http));
writeFileSync(`${out}/ws.jsonl`, jsonl(ws));
console.log(`http=${http.length} ws=${ws.length}`);
