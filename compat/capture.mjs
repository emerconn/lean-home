// Drive the real HA frontend in headless Chromium and record HTTP (/, /auth, /api, /.well-known) and WebSocket
// traffic as JSONL. Usage: [STEPS=json] node capture.mjs <ha-url> <out-dir>   (see capture.sh)
import { createRequire } from "node:module";
import { writeFileSync } from "node:fs";
import { execSync } from "node:child_process";
import { norm as normFor } from "./norm.mjs";
const puppeteer = createRequire(process.env.NODE_PATH + "/")("puppeteer-core");
const [url, out] = process.argv.slice(2);
const norm = (s) => normFor(s, url);
// Run after the dashboard loads, 3 s apart: {"call": [domain, service, data]} goes through the
// frontend's own connection, {"sh": "cmd"} runs on the host (e.g. a device going offline).
const steps = JSON.parse(process.env.STEPS ?? '[{"call": ["light", "toggle", {"entity_id": "light.kitchen"}]}]');
// The frontend posts /auth/token as multipart; keep only the fields.
const form = (s) => Object.fromEntries([...s.matchAll(/name="([^"]+)"\r\n\r\n([^\r]*)/g)].map((m) => [m[1], m[2]]));
const parse = (s) => { try { return JSON.parse(norm(s)); } catch { return norm(s); } };

const http = [], ws = [];
const browser = await puppeteer.launch({ executablePath: "/usr/bin/chromium", args: ["--no-sandbox"] });
const page = await browser.newPage();
await page.setViewport({ width: 1280, height: 900 });

page.on("response", async (res) => {
  const req = res.request(), p = new URL(req.url());
  if (p.origin !== new URL(url).origin) return;
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
cdp.on("Network.webSocketFrameSent", (e) => ws.push({ dir: "send", msg: parse(e.response.payloadData) }));
cdp.on("Network.webSocketFrameReceived", (e) => ws.push({ dir: "recv", msg: parse(e.response.payloadData) }));

await page.goto(url + "/", { waitUntil: "networkidle2" });
await page.locator("pierce/input[name=username]").fill("dev");
await page.locator("pierce/input[name=password]").fill("dev");
await page.keyboard.press("Enter");
await page.waitForSelector("pierce/hui-view", { timeout: 60000 });
await new Promise((r) => setTimeout(r, 5000));
for (const step of steps) {
  if (step.sh) execSync(step.sh, { stdio: "inherit" });
  else await page.evaluate((c) => document.querySelector("home-assistant").hass.callService(...c), step.call);
  await new Promise((r) => setTimeout(r, 3000));
}
await browser.close();

const jsonl = (a) => a.map((o) => JSON.stringify(o)).join("\n") + "\n";
writeFileSync(`${out}/http.jsonl`, jsonl(http));
writeFileSync(`${out}/ws.jsonl`, jsonl(ws));
console.log(`http=${http.length} ws=${ws.length}`);
