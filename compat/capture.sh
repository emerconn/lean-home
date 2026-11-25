#!/usr/bin/env bash
# Record one real HA frontend load (login → dashboard → toggle a light) into compat/fixtures/.
# Needs docker, curl, jq, node and /usr/bin/chromium. Temp files go to bench/.work/ (gitignored).
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); WORK=$ROOT/bench/.work/capture
. "$ROOT/compat/versions.env"
# HA_VER tries another release (e.g. while upgrading); the default is the pinned one.
IMG=${HA_VER:+ghcr.io/home-assistant/home-assistant:$HA_VER}; IMG=${IMG:-$HA_IMAGE}; PORT=${PORT:-18123}; URL=http://127.0.0.1:$PORT
OUT=$ROOT/compat/fixtures/frontend-load

docker rm -f lh-capture >/dev/null 2>&1 || true
# HA writes root-owned files, so clean up from inside a container.
mkdir -p "$WORK"; docker run --rm -v "$WORK:/w" --entrypoint rm "$IMG" -rf /w/config
mkdir -p "$WORK/config" "$OUT"
cat > "$WORK/config/configuration.yaml" <<'EOF'
default_config:
input_boolean:
  lamp:
    name: Lamp
template:
  - light:
      - name: Kitchen
        state: "{{ is_state('input_boolean.lamp', 'on') }}"
        turn_on: { action: input_boolean.turn_on, target: { entity_id: input_boolean.lamp } }
        turn_off: { action: input_boolean.turn_off, target: { entity_id: input_boolean.lamp } }
  - sensor:
      - name: Temperature
        state: "21.5"
        unit_of_measurement: "°C"
        device_class: temperature
EOF

docker run -d --name lh-capture -p "127.0.0.1:$PORT:8123" -e TZ=UTC -v "$WORK/config:/config" "$IMG" >/dev/null
trap 'docker rm -f lh-capture >/dev/null' EXIT
for i in $(seq 180); do curl -sf "$URL/api/onboarding" >/dev/null && break; sleep 1; done
curl -sf "$URL/api/onboarding" >/dev/null || { echo "HA did not start; docker logs lh-capture" >&2; exit 1; }

# Onboarding over REST, as the onboarding UI does it; the browser then logs in normally.
CID=$URL/
code=$(curl -sf "$URL/api/onboarding/users" -H 'content-type: application/json' \
  -d "{\"client_id\":\"$CID\",\"name\":\"Dev\",\"username\":\"dev\",\"password\":\"dev\",\"language\":\"en\"}" | jq -r .auth_code)
tok=$(curl -sf "$URL/auth/token" -d "grant_type=authorization_code&code=$code&client_id=$CID" | jq -r .access_token)
H=(-H "authorization: Bearer $tok" -H 'content-type: application/json')
curl -sf "${H[@]}" "$URL/api/onboarding/core_config" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/analytics" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/integration" -d "{\"client_id\":\"$CID\",\"redirect_uri\":\"${CID}?auth_callback=1\"}" >/dev/null

NODE_PATH=$ROOT/bench/.work/node/node_modules node "$ROOT/compat/capture.mjs" "$URL" "$OUT"
