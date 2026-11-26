#!/bin/sh
# Simulated MQTT devices for try/compose.yaml, the same ones the compat scenarios record
# (compat/mqtt-scenario.sh): one device "Demo Device" with two lights, a fan switch, a temperature
# sensor and a door. Runs in the Mosquitto image (busybox sh, mosquitto_pub/sub).
B="-h mosquitto"
pub() { mosquitto_pub $B -i lh-dev-pub -r -t "$1" -m "$2"; }
until mosquitto_pub $B -i lh-dev-probe -t lh/probe -n 2>/dev/null; do sleep 1; done

DEV='{"identifiers":["lh_demo"],"name":"Demo Device","manufacturer":"lean-home"}'
pub homeassistant/light/lh_demo/light1/config "{\"name\":\"Ceiling\",\"unique_id\":\"lh_light1\",\"state_topic\":\"lh/light1/state\",\"command_topic\":\"lh/light1/set\",\"brightness_state_topic\":\"lh/light1/brightness\",\"brightness_command_topic\":\"lh/light1/brightness/set\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub homeassistant/light/lh_demo/light2/config '{"~":"lh/light2","name":"Desk","uniq_id":"lh_light2","schema":"json","stat_t":"~/state","cmd_t":"~/set","brightness":true,"sup_clrm":["brightness"],"avty_t":"lh/status","dev":{"ids":["lh_demo"],"name":"Demo Device","mf":"lean-home"}}'
pub homeassistant/switch/lh_demo/switch1/config "{\"name\":\"Fan\",\"unique_id\":\"lh_switch1\",\"state_topic\":\"lh/switch1/state\",\"command_topic\":\"lh/switch1/set\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub homeassistant/sensor/lh_demo/temp/config "{\"name\":\"Temperature\",\"unique_id\":\"lh_temp\",\"state_topic\":\"lh/sensor1/state\",\"value_template\":\"{{ value_json.temperature }}\",\"device_class\":\"temperature\",\"unit_of_measurement\":\"°C\",\"state_class\":\"measurement\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub homeassistant/binary_sensor/lh_demo/door/config "{\"name\":\"Front door\",\"unique_id\":\"lh_door\",\"state_topic\":\"lh/door/state\",\"device_class\":\"door\",\"payload_on\":\"ON\",\"payload_off\":\"OFF\",\"availability_topic\":\"lh/status\",\"device\":$DEV}"
pub lh/status online
pub lh/light1/state OFF
pub lh/light1/brightness 255
pub lh/light2/state '{"state":"OFF"}'
pub lh/switch1/state OFF
pub lh/door/state OFF

# The temperature drifts between 19.0 and 23.0 °C, a step every 30 s.
(t=210; while true; do
  t=$(( t + RANDOM % 5 - 2 )); [ $t -lt 190 ] && t=190; [ $t -gt 230 ] && t=230
  pub lh/sensor1/state "{\"temperature\":$((t / 10)).$((t % 10))}"
  sleep 30
done) &

# Commands come back as state, as a real device confirms them.
mosquitto_sub $B -i lh-dev-sub -t 'lh/+/set' -t 'lh/+/brightness/set' -F '%t %p' |
  while read -r topic payload; do
    case $topic in
      lh/light1/set) pub lh/light1/state "$payload" ;;
      lh/light1/brightness/set) pub lh/light1/brightness "$payload" ;;
      # The JSON light reports its color mode while on.
      lh/light2/set) case $payload in
          *'"ON"'*) pub lh/light2/state "${payload%\}},\"color_mode\":\"brightness\"}" ;;
          *) pub lh/light2/state "$payload" ;;
        esac ;;
      lh/switch1/set) pub lh/switch1/state "$payload" ;;
    esac
  done
