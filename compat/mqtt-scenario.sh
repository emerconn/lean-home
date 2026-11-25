# The MQTT discovery scenario, shared by capture-mqtt.sh (records HA) and replay.sh (replays it
# against lean-home) so both run the exact same devices and steps. Source it with $MQ set to the
# Mosquitto container's name.

pub() { docker exec "$MQ" mosquitto_pub -i lh-pub -r -t "$1" -m "$2"; }

# Every message, in broker order; v5 retain-as-published keeps the retain flag on live messages.
# usage: mqtt_logger > file &
mqtt_logger() { docker exec "$MQ" mosquitto_sub -i lh-logger -V mqttv5 --retain-as-published -t '#' -F '%r	%q	%t	%p'; }

# A device that confirms commands: HA isn't optimistic when a state topic exists. usage: mqtt_responder &
mqtt_responder() {
  docker exec "$MQ" mosquitto_sub -i lh-responder -t 'lh/+/set' -t 'lh/+/brightness/set' -F '%t %p' |
    while read -r topic payload; do
      case $topic in
        lh/light1/set) pub lh/light1/state "$payload" ;;
        lh/light1/brightness/set) pub lh/light1/brightness "$payload" ;;
        lh/light2/set) pub lh/light2/state "$(jq -c 'if .state == "ON" then . + {color_mode: "brightness"} else . end' <<< "$payload")" ;;
        lh/switch1/set) pub lh/switch1/state "$payload" ;;
      esac
    done
}

# Retained discovery configs and initial states, published before the hub starts.
mqtt_devices() {
  local DEV='{"identifiers":["lh_demo"],"name":"Demo Device","manufacturer":"lean-home"}'
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
}

# The browser steps (capture.mjs STEPS): commands through the frontend, then the device changes
# and goes offline. The sleep keeps the offline write over 5 s after the last service call: HA reuses a
# call's context that long (CONTEXT_RECENT_TIME_SECONDS), and the recording has a fresh one there.
mqtt_steps() {
  cat <<EOF
[{"call": ["light", "toggle", {"entity_id": "light.demo_device_ceiling"}]},
 {"call": ["light", "turn_on", {"entity_id": "light.demo_device_ceiling", "brightness_pct": 50}]},
 {"call": ["light", "toggle", {"entity_id": "light.demo_device_desk"}]},
 {"call": ["switch", "toggle", {"entity_id": "switch.demo_device_fan"}]},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/sensor1/state -m '{\"temperature\":22.0}'"},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/door/state -m ON"},
 {"sh": "sleep 5"},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/status -m offline"}]
EOF
}

# mqtt.tsv (from mqtt_logger) -> mqtt.jsonl. usage: mqtt_jsonl < tsv > jsonl
mqtt_jsonl() { jq -cR 'split("\t") | {topic: .[2], payload: (.[3:] | join("\t")), retain: (.[0] == "1"), qos: (.[1] | tonumber)}'; }

# The automations scenario's browser steps, on the same devices plus compat/automations/*.yaml:
# a live template, failing ones (errors with and without report_errors, strict mode) and one that renders with
# an undefined variable, automations firing from state and MQTT triggers, a script with delay and
# choose, then the frontend's automation, trace and script pages (with the script's traces).
automations_steps() {
  local T="{{ states('sensor.demo_device_temperature') }}|{{ is_state('binary_sensor.demo_device_front_door', 'on') }}|{{ state_attr('light.demo_device_ceiling', 'brightness') }}|{{ states('sensor.missing') | float(0) }}|{{ iif(is_state('switch.demo_device_fan', 'on'), 'fan', 'still') }}"
  cat <<EOF
[{"sub": {"type": "render_template", "template": $(jq -Rn --arg t "$T" '$t')}},
 {"sub": {"type": "render_template", "template": "{{ 'abc' | float }}", "report_errors": true}},
 {"sub": {"type": "render_template", "template": "{{ nope.attr }}", "report_errors": true}},
 {"sub": {"type": "render_template", "template": "{{ 'abc' | float }}"}},
 {"sub": {"type": "render_template", "template": "{{ nope }}", "strict": true, "report_errors": true}},
 {"sub": {"type": "render_template", "template": "{{ nope }} x", "report_errors": true}},
 {"call": ["switch", "turn_on", {"entity_id": "switch.demo_device_fan"}]},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -t lh/button -m press"},
 {"call": ["switch", "turn_off", {"entity_id": "switch.demo_device_fan"}]},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/door/state -m ON"},
 {"call": ["automation", "trigger", {"entity_id": "automation.fan_lights_ceiling", "skip_condition": false}]},
 {"call": ["script", "blink", {"target_light": "light.demo_device_ceiling"}]},
 {"sh": "docker exec $MQ mosquitto_pub -i lh-pub -r -t lh/sensor1/state -m '{\"temperature\":22.0}'"},
 {"call": ["automation", "turn_off", {"entity_id": "automation.nightly_off"}]},
 {"nav": "/config/automation/dashboard"},
 {"nav": "/config/automation/edit/1700000000001"},
 {"nav": "/config/automation/trace/1700000000001"},
 {"nav": "/config/script/dashboard"},
 {"nav": "/config/script/edit/blink"},
 {"nav": "/config/script/trace/blink"}]
EOF
}
