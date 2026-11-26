#!/usr/bin/env bash
# One-time setup for try/compose.yaml: writes .env with the pinned versions, then onboards a fresh
# config dir with stock Home Assistant (lean-home has no onboarding yet) and adds its MQTT
# integration pointed at the compose broker, plus the compat scenario's example automations and
# script. Needs docker, curl and jq.
#   usage: try/setup.sh [username]     (asks for the password)
#          try/setup.sh reset          (stops everything and deletes config/ to start over)
set -euo pipefail
cd "$(dirname "$0")"
. ../compat/versions.env
if [ "${1:-}" = reset ]; then
  docker compose down 2>/dev/null || true
  # HA's container wrote config/ as root: delete it through a container too.
  [ -d config ] && docker run --rm -v "$PWD:/try" --entrypoint rm "$MOSQUITTO_IMAGE" -rf /try/config
  exit 0
fi
USERNAME=${1:-dev}
printf 'HA_FRONTEND=%s\nMOSQUITTO_IMAGE=%s\n' "$HA_FRONTEND" "$MOSQUITTO_IMAGE" > .env
if [ -f config/.storage/onboarding ]; then
  echo "config/ is already onboarded; run ./setup.sh reset to start over."
  exit 0
fi
if [ -d config ]; then
  echo "config/ exists but isn't onboarded (an earlier setup failed?): run ./setup.sh reset first." >&2
  exit 1
fi
read -rsp "Password for $USERNAME: " PASSWORD; echo
[ -n "$PASSWORD" ] || { echo "a password is required" >&2; exit 1; }
mkdir -p config

docker compose up -d mosquitto
HA=lean-home-try-onboard PORT=18123 URL=http://127.0.0.1:18123
docker rm -f "$HA" >/dev/null 2>&1 || true
trap 'docker rm -f "$HA" >/dev/null 2>&1 || true' EXIT
TZ_NAME=$(timedatectl show -p Timezone --value 2>/dev/null || cat /etc/timezone 2>/dev/null || echo UTC)
echo "Onboarding with $HA_IMAGE (a minute or two)..."
docker run -d --name "$HA" --network lean-home-try_default -p "127.0.0.1:$PORT:8123" \
  -e TZ="$TZ_NAME" -v "$PWD/config:/config" "$HA_IMAGE" >/dev/null
for _ in $(seq 180); do curl -sf "$URL/api/onboarding" >/dev/null && break; sleep 1; done
curl -sf "$URL/api/onboarding" >/dev/null || { echo "HA did not start: docker logs $HA" >&2; exit 1; }

# Onboarding over REST, as compat/capture-mqtt.sh does it.
CID=$URL/
body=$(jq -n --arg u "$USERNAME" --arg p "$PASSWORD" --arg c "$CID" \
  '{client_id: $c, name: $u, username: $u, password: $p, language: "en"}')
code=$(curl -sf "$URL/api/onboarding/users" -H 'content-type: application/json' -d "$body" | jq -r .auth_code)
tok=$(curl -sf "$URL/auth/token" -d "grant_type=authorization_code&code=$code&client_id=$CID" | jq -r .access_token)
H=(-H "authorization: Bearer $tok" -H 'content-type: application/json')
curl -sf "${H[@]}" "$URL/api/onboarding/core_config" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/analytics" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/integration" -d "{\"client_id\":\"$CID\",\"redirect_uri\":\"${CID}?auth_callback=1\"}" >/dev/null
flow=$(curl -sf "${H[@]}" "$URL/api/config/config_entries/flow" -d '{"handler":"mqtt"}' | jq -r .flow_id)
result=$(curl -s "${H[@]}" "$URL/api/config/config_entries/flow/$flow" -d '{"broker":"mosquitto","port":1883,"other_settings":{"set_ca_cert":"off","set_client_cert":false}}')
[ "$(jq -r .type <<< "$result")" = create_entry ] || { echo "MQTT setup failed: $result" >&2; exit 1; }
# HA writes .storage with a delay, and a stop signal doesn't flush what's pending (its s6
# supervisor ends it): wait for the files lean-home reads, then stop it.
for _ in $(seq 60); do
  [ -f config/.storage/auth ] && [ -f config/.storage/core.config_entries ] && break
  sleep 1
done
[ -f config/.storage/auth ] && [ -f config/.storage/core.config_entries ] \
  || { echo "HA didn't write config/.storage/auth and core.config_entries: docker logs $HA" >&2; exit 1; }
sleep 2 # whatever was saved in the same batch
docker stop -t 120 "$HA" >/dev/null

# Examples to try: the compat scenario's automations and script, written for these devices.
# HA's container wrote config/ as root, so copy them in through a container too.
docker run --rm -v "$PWD/config:/config" -v "$PWD/../compat/automations:/examples:ro" \
  --entrypoint sh "$MOSQUITTO_IMAGE" -c "cp /examples/automations.yaml /examples/scripts.yaml /config/ \
  && printf 'homeassistant:\n  time_zone: %s\n' '$TZ_NAME' >> /config/configuration.yaml"
echo "Done. Start it with: docker compose up -d --build   (then open http://localhost:8123)"
