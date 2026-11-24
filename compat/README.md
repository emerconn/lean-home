# compat

Recorded traffic from real Home Assistant, used as the reference that lean-home must match.

`fixtures/frontend-load/` is one frontend session against HA **2026.11.2** (frontend **20261028.6**):
log in through the real login form, wait for the dashboard, toggle `light.kitchen`.
`http.jsonl` holds the `/`, `/auth/*` and `/api/*` requests, `ws.jsonl` every WebSocket frame
(`send`/`recv`), and `index.html` the page as HA renders it. Tokens, auth codes, PKCE values,
timestamps (ISO → `<ts>`, epoch → `0`), ULIDs and 32-hex ids are replaced with placeholders.
HA coalesces WS messages into arrays depending on timing, so compare per message, not per frame.

Re-record with `compat/capture.sh` (needs docker, node, `/usr/bin/chromium`, and
`npm i puppeteer-core` in `bench/.work/node`). Config and temp files go to `bench/.work/capture`.
