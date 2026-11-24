#!/usr/bin/env bash
# Record real HA with MQTT discovery (Mosquitto, five discovered entities on one device) into
# compat/fixtures/mqtt-discovery/: the frontend's traffic, every MQTT message, HA's subscriptions,
# and the .storage files HA wrote. Needs docker, curl, jq, node and /usr/bin/chromium.
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/.." && pwd); WORK=$ROOT/bench/.work/capture-mqtt
IMG=ghcr.io/home-assistant/home-assistant:${HA_VER:-2026.11.2}; MQTT_IMG=eclipse-mosquitto:2
PORT=${PORT:-18123}; URL=http://127.0.0.1:$PORT; OUT=$ROOT/compat/fixtures/mqtt-discovery
NET=lh-mqtt; HA=lh-mqtt-ha; MQ=lh-mosq

cleanup() { docker rm -f "$HA" "$MQ" >/dev/null 2>&1 || true; docker network rm "$NET" >/dev/null 2>&1 || true; }
cleanup
# HA writes root-owned files, so clean up from inside a container.
mkdir -p "$WORK"; docker run --rm -v "$WORK:/w" --entrypoint rm "$IMG" -rf /w/config /w/mosquitto
mkdir -p "$WORK/config" "$WORK/mosquitto" "$OUT"
echo "default_config:" > "$WORK/config/configuration.yaml"
cat > "$WORK/mosquitto/mosquitto.conf" <<'EOF'
listener 1883
allow_anonymous true
log_dest stdout
log_type all
log_timestamp false
EOF

docker network create "$NET" >/dev/null
trap 'kill $(jobs -p) 2>/dev/null || true; cleanup' EXIT
docker run -d --name "$MQ" --network "$NET" -v "$WORK/mosquitto/mosquitto.conf:/mosquitto/config/mosquitto.conf:ro" "$MQTT_IMG" >/dev/null
pub() { docker exec "$MQ" mosquitto_pub -i lh-pub -r -t "$1" -m "$2"; }
sleep 1

# Every message, in broker order; v5 retain-as-published keeps the retain flag on live messages.
docker exec "$MQ" mosquitto_sub -i lh-logger -V mqttv5 --retain-as-published -t '#' -F '%r	%q	%t	%p' > "$WORK/mqtt.tsv" &
# A device that confirms commands: HA isn't optimistic when a state topic exists.
docker exec "$MQ" mosquitto_sub -i lh-responder -t 'lh/+/set' -t 'lh/+/brightness/set' -F '%t %p' |
  while read -r topic payload; do
    case $topic in
      lh/light1/set) pub lh/light1/state "$payload" ;;
      lh/light1/brightness/set) pub lh/light1/brightness "$payload" ;;
      lh/light2/set) pub lh/light2/state "$(jq -c 'if .state == "ON" then . + {color_mode: "brightness"} else . end' <<< "$payload")" ;;
      lh/switch1/set) pub lh/switch1/state "$payload" ;;
    esac
  done &
sleep 1

DEV='{"identifiers":["lh_demo"],"name":"Demo Device","manufacturer":"lean-home"}'
pub homeassistant/light/lh_demo/light1/config "{\"name\":\"Ceiling\",\"unique_id\":\"lh_light1\",\"state_topic\":\"lh/light1/state\",\"command_topic\":\"lh/light1/set\",\"brightness_state_topic\":\"lh/light1/brightness\",\"brightness_command_topic\":\"lh/light1/brightness/set\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
# Abbreviated keys and a ~ base topic, as Tasmota and Zigbee2MQTT send them.
pub homeassistant/light/lh_demo/light2/config '{"~":"lh/light2","name":"Desk","uniq_id":"lh_light2","schema":"json","stat_t":"~/state","cmd_t":"~/set","brightness":true,"sup_clrm":["brightness"],"avty_t":"lh/status","dev":{"ids":["lh_demo"],"name":"Demo Device","mf":"lean-home"}}'
pub homeassistant/switch/lh_demo/switch1/config "{\"name\":\"Fan\",\"unique_id\":\"lh_switch1\",\"state_topic\":\"lh/switch1/state\",\"command_topic\":\"lh/switch1/set\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub homeassistant/sensor/lh_demo/temp/config "{\"name\":\"Temperature\",\"unique_id\":\"lh_temp\",\"state_topic\":\"lh/sensor1/state\",\"value_template\":\"{{ value_json.temperature }}\",\"device_class\":\"temperature\",\"unit_of_measurement\":\"°C\",\"state_class\":\"measurement\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub homeassistant/binary_sensor/lh_demo/door/config "{\"name\":\"Front door\",\"unique_id\":\"lh_door\",\"state_topic\":\"lh/door/state\",\"device_class\":\"door\",\"payload_on\":\"ON\",\"payload_off\":\"OFF\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub lh/status online
pub lh/light1/state OFF
pub lh/light1/brightness 255
pub lh/light2/state '{"state":"OFF"}'
pub lh/switch1/state OFF
pub lh/sensor1/state '{"temperature":21.5}'
pub lh/door/state OFF

docker run -d --name "$HA" --network "$NET" -p "127.0.0.1:$PORT:8123" -e TZ=UTC -v "$WORK/config:/config" "$IMG" >/dev/null
for _ in $(seq 180); do curl -sf "$URL/api/onboarding" >/dev/null && break; sleep 1; done
curl -sf "$URL/api/onboarding" >/dev/null || { echo "HA did not start; docker logs $HA" >&2; exit 1; }

# Onboarding over REST, as capture.sh does it.
CID=$URL/
code=$(curl -sf "$URL/api/onboarding/users" -H 'content-type: application/json' \
  -d "{\"client_id\":\"$CID\",\"name\":\"Dev\",\"username\":\"dev\",\"password\":\"dev\",\"language\":\"en\"}" | jq -r .auth_code)
tok=$(curl -sf "$URL/auth/token" -d "grant_type=authorization_code&code=$code&client_id=$CID" | jq -r .access_token)
H=(-H "authorization: Bearer $tok" -H 'content-type: application/json')
curl -sf "${H[@]}" "$URL/api/onboarding/core_config" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/analytics" -d '{}' >/dev/null
curl -sf "${H[@]}" "$URL/api/onboarding/integration" -d "{\"client_id\":\"$CID\",\"redirect_uri\":\"${CID}?auth_callback=1\"}" >/dev/null

# The MQTT integration through its config flow, defaults except the broker.
flow=$(curl -sf "${H[@]}" "$URL/api/config/config_entries/flow" -d '{"handler":"mqtt"}' | jq -r .flow_id)
result=$(curl -s "${H[@]}" "$URL/api/config/config_entries/flow/$flow" -d "{\"broker\":\"$MQ\",\"port\":1883,\"other_settings\":{\"set_ca_cert\":\"off\",\"set_client_cert\":false}}")
[ "$(jq -r .type <<< "$result")" = create_entry ] || { echo "mqtt config flow failed: $result" >&2; exit 1; }
sleep 5

STEPS=$(cat <<EOF
[{"call": ["light", "toggle", {"entity_id": "light.demo_device_ceiling"}]},
 {"call": ["light", "turn_on", {"entity_id": "light.demo_device_ceiling", "brightness_pct": 50}]},
 {"call": ["light", "toggle", {"entity_id": "light.demo_device_desk"}]},
 {"call": ["switch", "toggle", {"entity_id": "switch.demo_device_fan"}]},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/sensor1/state -m '{\"temperature\":22.0}'"},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/door/state -m ON"},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/status -m offline"}]
EOF
)
STEPS=$STEPS NODE_PATH=$ROOT/bench/.work/node/node_modules node "$ROOT/compat/capture.mjs" "$URL" "$OUT"
rm -f "$OUT/index.html" # same page as frontend-load

# Stopping HA flushes its delayed .storage writes.
docker stop "$HA" >/dev/null
for f in core.config_entries core.entity_registry core.device_registry; do
  docker run --rm -v "$WORK/config:/c:ro" --entrypoint cat "$IMG" "/c/.storage/$f" > "$WORK/$f"
  node "$ROOT/compat/norm.mjs" "$URL" "$WORK/$f" "$OUT/$f.json"
done
jq -cR 'split("\t") | {topic: .[2], payload: (.[3:] | join("\t")), retain: (.[0] == "1"), qos: (.[1] | tonumber)}' "$WORK/mqtt.tsv" > "$WORK/mqtt.jsonl"
node "$ROOT/compat/norm.mjs" "$URL" "$WORK/mqtt.jsonl" "$OUT/mqtt.jsonl"
# HA's subscriptions, in order: mosquitto logs each granted topic as "<client-id> <qos> <topic>";
# our own tools' client ids start with lh-.
docker logs "$MQ" 2>&1 | awk 'NF == 3 && $2 ~ /^[012]$/ && $1 !~ /^lh-/ { print $3 " (QoS " $2 ")" }' > "$OUT/broker-subscriptions.txt"
docker logs "$MQ" > "$WORK/broker.log" 2>&1
echo "mqtt=$(wc -l < "$OUT/mqtt.jsonl") subscriptions=$(wc -l < "$OUT/broker-subscriptions.txt")"
