//! Blueprints (HA's `components/blueprint/models.py` and `annotatedyaml.input`): loading
//! `blueprints/<domain>/<path>`, flattening sectioned inputs, `!input` extraction and
//! substitution, and `use_blueprint` expansion.

use std::collections::BTreeSet;
use std::path::{Component, Path};

use serde_json::{Map, Value, json};

/// A parsed blueprint YAML file.
#[derive(Debug, Clone)]
pub struct Blueprint {
    pub data: Map<String, Value>,
    pub name: String,
    /// Flattened `blueprint.input` definitions (sectioned `input:` blocks expanded one level).
    pub inputs: Map<String, Value>,
}

impl Blueprint {
    /// Load `<config_dir>/blueprints/<domain>/<rel_path>` and validate its metadata and inputs.
    pub fn load(config_dir: &Path, domain: &str, rel_path: &str) -> Result<Self, String> {
        if !is_safe_rel_path(rel_path) {
            return Err(format!("invalid blueprint path: {rel_path}"));
        }
        let path = config_dir.join("blueprints").join(domain).join(rel_path);
        let val =
            crate::config::load_file(&path)?.ok_or_else(|| format!("Unable to find {rel_path}"))?;
        Self::from_value(val, Some(domain))
    }

    /// Parse a blueprint from a loaded YAML `Value`.
    pub fn from_value(val: Value, expected_domain: Option<&str>) -> Result<Self, String> {
        let Value::Object(data) = val else {
            return Err("blueprint: expected a dictionary".into());
        };
        let meta = data
            .get("blueprint")
            .and_then(Value::as_object)
            .ok_or("required key not provided @ data['blueprint']")?;
        let name = meta
            .get("name")
            .and_then(Value::as_str)
            .ok_or("required key not provided @ data['blueprint']['name']")?
            .to_string();
        let domain = meta
            .get("domain")
            .and_then(Value::as_str)
            .ok_or("required key not provided @ data['blueprint']['domain']")?
            .to_string();
        if let Some(expected) = expected_domain
            && domain != expected
        {
            return Err(format!(
                "Found incorrect blueprint type {domain}, expected {expected}"
            ));
        }

        let mut inputs = Map::new();
        match meta.get("input") {
            None | Some(Value::Null) => {}
            Some(Value::Object(raw_inputs)) => {
                for (k, v) in raw_inputs {
                    if let Some(sub) = v
                        .as_object()
                        .and_then(|m| m.get("input"))
                        .and_then(Value::as_object)
                    {
                        for (sk, sv) in sub {
                            inputs.insert(sk.clone(), sv.clone());
                        }
                    } else {
                        inputs.insert(k.clone(), v.clone());
                    }
                }
            }
            Some(_) => return Err("blueprint.input: expected a dictionary".into()),
        }

        let mut referenced = BTreeSet::new();
        extract_inputs(&Value::Object(data.clone()), &mut referenced);
        let missing: Vec<String> = referenced
            .into_iter()
            .filter(|name| !inputs.contains_key(name))
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Missing input definition for {}",
                missing.join(", ")
            ));
        }

        Ok(Self { data, name, inputs })
    }

    pub fn metadata(&self) -> &Value {
        &self.data["blueprint"]
    }

    /// Substitute `!input` tags using `use_blueprint.input` + defaults, then merge the caller's
    /// top-level keys (`id`, `alias`, …) and strip `blueprint` / `use_blueprint`.
    pub fn substitute(
        &self,
        raw_config: &Map<String, Value>,
    ) -> Result<Map<String, Value>, String> {
        let bp_conf = raw_config
            .get("use_blueprint")
            .and_then(Value::as_object)
            .ok_or("use_blueprint: expected a dictionary")?;
        let mut resolved = match bp_conf.get("input") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(m)) => m.clone(),
            Some(_) => return Err("use_blueprint.input: expected a dictionary".into()),
        };

        for (name, def) in &self.inputs {
            if !resolved.contains_key(name)
                && let Some(default) = def.as_object().and_then(|m| m.get("default"))
            {
                resolved.insert(name.clone(), default.clone());
            }
        }

        let missing: Vec<&str> = self
            .inputs
            .keys()
            .filter(|k| !resolved.contains_key(k.as_str()))
            .map(String::as_str)
            .collect();
        if !missing.is_empty() {
            return Err(format!(
                "Missing input {} for blueprint {}",
                missing.join(", "),
                self.name
            ));
        }

        let Value::Object(mut combined) =
            substitute_value(&Value::Object(self.data.clone()), &resolved)?
        else {
            unreachable!("blueprint data is always an object");
        };
        combined.remove("blueprint");
        for (k, v) in raw_config {
            combined.insert(k.clone(), v.clone());
        }
        combined.remove("use_blueprint");
        Ok(combined)
    }
}

/// If `raw` uses a blueprint (`use_blueprint:`), load and expand it, returning
/// `(expanded_config, Some(raw_blueprint_inputs))`. Otherwise returns `(raw.clone(), None)`.
pub fn expand_automation(config_dir: &Path, raw: &Value) -> Result<(Value, Option<Value>), String> {
    let Some(raw_obj) = raw.as_object() else {
        return Ok((raw.clone(), None));
    };
    let Some(bp_conf) = raw_obj.get("use_blueprint") else {
        return Ok((raw.clone(), None));
    };
    let bp_obj = bp_conf
        .as_object()
        .ok_or("use_blueprint: expected a dictionary")?;
    let path = bp_obj
        .get("path")
        .and_then(Value::as_str)
        .ok_or("required key not provided @ data['use_blueprint']['path']")?;
    let bp = Blueprint::load(config_dir, "automation", path)?;
    let expanded = bp.substitute(raw_obj)?;
    Ok((Value::Object(expanded), Some(raw.clone())))
}

/// List all blueprints under `<config_dir>/blueprints/<domain>` for `blueprint/list`.
pub fn list(config_dir: &Path, domain: &str) -> Map<String, Value> {
    let mut out = Map::new();
    if !is_safe_rel_path(domain) {
        return out;
    }
    let dir = config_dir.join("blueprints").join(domain);
    let mut files = vec![];
    crate::config::find_yaml_files(&dir, &mut files);
    for file in files {
        let Ok(rel) = file.strip_prefix(&dir) else {
            continue;
        };
        let key = rel.to_string_lossy().replace('\\', "/");
        let entry = match crate::config::load_file(&file)
            .and_then(|opt| opt.ok_or_else(|| format!("Unable to find {key}")))
            .and_then(|v| Blueprint::from_value(v, Some(domain)))
        {
            Ok(bp) => json!({"metadata": bp.metadata()}),
            Err(e) => json!({"error": e}),
        };
        out.insert(key, entry);
    }
    out
}

fn is_safe_rel_path(path: &str) -> bool {
    !path.is_empty()
        && !Path::new(path).components().any(|c| {
            matches!(
                c,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
}

fn input_tag(m: &Map<String, Value>) -> Option<&str> {
    if m.len() == 1 {
        m.get("!input").and_then(Value::as_str)
    } else {
        None
    }
}

fn extract_inputs(val: &Value, found: &mut BTreeSet<String>) {
    match val {
        Value::Object(m) => {
            if let Some(name) = input_tag(m) {
                found.insert(name.to_string());
                return;
            }
            for v in m.values() {
                extract_inputs(v, found);
            }
        }
        Value::Array(a) => {
            for v in a {
                extract_inputs(v, found);
            }
        }
        _ => {}
    }
}

fn substitute_value(val: &Value, inputs: &Map<String, Value>) -> Result<Value, String> {
    match val {
        Value::Object(m) => {
            if let Some(name) = input_tag(m) {
                return inputs
                    .get(name)
                    .cloned()
                    .ok_or_else(|| format!("No substitution found for input {name}"));
            }
            let mut out = Map::with_capacity(m.len());
            for (k, v) in m {
                out.insert(k.clone(), substitute_value(v, inputs)?);
            }
            Ok(Value::Object(out))
        }
        Value::Array(a) => {
            let mut out = Vec::with_capacity(a.len());
            for v in a {
                out.push(substitute_value(v, inputs)?);
            }
            Ok(Value::Array(out))
        }
        _ => Ok(val.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::automation::Automations;
    use crate::registry::Registry;
    use crate::restore::Restore;
    use crate::state::{Context, States};

    const MOTION_LIGHT_YAML: &str = r#"blueprint:
  name: Motion-activated Light
  description: Turn on a light when motion is detected.
  domain: automation
  source_url: https://github.com/home-assistant/core/blob/dev/homeassistant/components/automation/blueprints/motion_light.yaml
  author: Home Assistant
  input:
    motion_entity:
      name: Motion Sensor
      selector:
        entity:
          filter:
            - device_class: occupancy
              domain: binary_sensor
            - device_class: motion
              domain: binary_sensor
    light_target:
      name: Light
      selector:
        target:
          entity:
            domain: light
    no_motion_wait:
      name: Wait time
      description: Time to leave the light on after last motion is detected.
      default: 120
      selector:
        number:
          min: 0
          max: 3600
          unit_of_measurement: seconds

mode: restart
max_exceeded: silent

triggers:
  trigger: state
  entity_id: !input motion_entity
  from: "off"
  to: "on"

actions:
  - alias: "Turn on the light"
    action: light.turn_on
    target: !input light_target
  - alias: "Wait until there is no motion from device"
    wait_for_trigger:
      trigger: state
      entity_id: !input motion_entity
      from: "on"
      to: "off"
  - alias: "Wait the number of seconds that has been set"
    delay: !input no_motion_wait
  - alias: "Turn off the light"
    action: light.turn_off
    target: !input light_target
"#;

    const NOTIFY_LEAVING_ZONE_YAML: &str = r#"blueprint:
  name: Zone Notification
  description: Send a notification to a device when a person leaves a specific zone.
  domain: automation
  source_url: https://github.com/home-assistant/core/blob/dev/homeassistant/components/automation/blueprints/notify_leaving_zone.yaml
  author: Home Assistant
  input:
    person_entity:
      name: Person
      selector:
        entity:
          filter:
            domain: person
    zone_entity:
      name: Zone
      selector:
        entity:
          filter:
            domain: zone
    notify_device:
      name: Device to notify
      description: Device needs to run the official Home Assistant app to receive notifications.
      selector:
        device:
          filter:
            integration: mobile_app

triggers:
  trigger: state
  entity_id: !input person_entity

variables:
  zone_entity: !input zone_entity
  zone_state: "{{ states[zone_entity].name }}"
  person_entity: !input person_entity
  person_name: "{{ states[person_entity].name }}"

conditions:
  condition: template
  value_template: "{{ zone_entity == 'zone.home' and trigger.from_state.state == 'home' and trigger.to_state.state != 'home' or trigger.from_state.state == zone_state and trigger.to_state.state != zone_state }}"

actions:
  - alias: "Notify that a person has left the zone"
    domain: mobile_app
    type: notify
    device_id: !input notify_device
    message: "{{ person_name }} has left {{ zone_state }}"
"#;

    #[test]
    fn default_blueprints_match_ha_fixture() {
        let dir = crate::e2e_tests::fixture_dir();
        let bp_dir = dir.join("blueprints/automation/homeassistant");
        std::fs::create_dir_all(&bp_dir).unwrap();
        std::fs::write(bp_dir.join("motion_light.yaml"), MOTION_LIGHT_YAML).unwrap();
        std::fs::write(
            bp_dir.join("notify_leaving_zone.yaml"),
            NOTIFY_LEAVING_ZONE_YAML,
        )
        .unwrap();

        let fixture: Value = serde_json::from_str(include_str!(
            "../../../compat/fixtures/step2/pr4-blueprints.json"
        ))
        .unwrap();

        let raw_motion_default = json!({
            "id": "bp_motion_light_default",
            "alias": "Motion Light (default wait)",
            "use_blueprint": {
                "path": "homeassistant/motion_light.yaml",
                "input": {
                    "motion_entity": "binary_sensor.hall_motion",
                    "light_target": {"entity_id": "light.hall"},
                },
            },
        });
        let (expanded_default, bp_inputs_default) =
            expand_automation(&dir, &raw_motion_default).unwrap();
        assert_eq!(bp_inputs_default, Some(raw_motion_default));
        assert_eq!(
            expanded_default,
            fixture["validated_motion_default_raw_config"]
        );

        let raw_motion_zero =
            fixture["traces"]["automation.bp_motion_light_zero"][0]["extended_dict"]
                ["blueprint_inputs"]
                .clone();
        let (expanded_zero, _) = expand_automation(&dir, &raw_motion_zero).unwrap();
        assert_eq!(
            expanded_zero,
            fixture["traces"]["automation.bp_motion_light_zero"][0]["extended_dict"]["config"]
        );

        let raw_zone_notify = json!({
            "id": "bp_zone_notify",
            "alias": "Notify Leaving Home",
            "use_blueprint": {
                "path": "homeassistant/notify_leaving_zone.yaml",
                "input": {
                    "person_entity": "person.alice",
                    "zone_entity": "zone.home",
                    "notify_device": "fake_device_id_123",
                },
            },
        });
        let (expanded_zone, _) = expand_automation(&dir, &raw_zone_notify).unwrap();
        assert_eq!(expanded_zone, fixture["substituted_zone_notify_config"]);

        let listed = list(&dir, "automation");
        assert_eq!(
            listed["homeassistant/motion_light.yaml"]["metadata"]["name"],
            "Motion-activated Light"
        );
        assert_eq!(
            listed["homeassistant/notify_leaving_zone.yaml"]["metadata"]["name"],
            "Zone Notification"
        );

        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn nested_inputs_and_errors() {
        let dir = crate::e2e_tests::fixture_dir();
        let bp_dir = dir.join("blueprints/automation/custom");
        std::fs::create_dir_all(&bp_dir).unwrap();
        std::fs::write(
            bp_dir.join("sectioned.yaml"),
            r#"blueprint:
  name: Sectioned
  domain: automation
  input:
    grp:
      name: Group
      input:
        ent:
          name: Entity
        delay_s:
          name: Delay
          default: 5
triggers:
  - trigger: state
    entity_id: !input ent
actions:
  - delay: !input delay_s
"#,
        )
        .unwrap();
        std::fs::write(
            bp_dir.join("bad_missing_def.yaml"),
            r#"blueprint:
  name: Bad
  domain: automation
  input: {}
triggers:
  - trigger: state
    entity_id: !input undeclared
actions: []
"#,
        )
        .unwrap();

        let raw = json!({
            "id": "sec1",
            "alias": "Sectioned Test",
            "use_blueprint": {
                "path": "custom/sectioned.yaml",
                "input": {"ent": "binary_sensor.door"}
            }
        });
        let (expanded, _) = expand_automation(&dir, &raw).unwrap();
        assert_eq!(expanded["triggers"][0]["entity_id"], "binary_sensor.door");
        assert_eq!(expanded["actions"][0]["delay"], 5);

        let missing = json!({
            "use_blueprint": {"path": "custom/sectioned.yaml", "input": {}}
        });
        assert!(
            expand_automation(&dir, &missing)
                .unwrap_err()
                .contains("Missing input ent")
        );
        assert!(
            Blueprint::load(&dir, "automation", "custom/bad_missing_def.yaml")
                .unwrap_err()
                .contains("Missing input definition for undeclared")
        );
        assert!(
            Blueprint::load(&dir, "automation", "../secrets.yaml")
                .unwrap_err()
                .contains("invalid blueprint path")
        );
        assert!(
            Blueprint::load(&dir, "script", "custom/sectioned.yaml")
                .unwrap_err()
                .contains("Unable to find")
        );

        let listed = list(&dir, "automation");
        assert!(listed["custom/bad_missing_def.yaml"]["error"].is_string());

        // Automation::new marks missing blueprint as unavailable instead of panicking.
        let reg = Registry::default();
        let states = States::default();
        let bad_auto = json!({
            "id": "missing_bp",
            "alias": "Missing BP",
            "use_blueprint": {"path": "custom/nope.yaml", "input": {}}
        });
        Automations::load(
            &[bad_auto],
            &dir,
            &reg,
            &states,
            &Restore::load(std::path::Path::new("/nonexistent")),
        );
        assert_eq!(
            states.get("automation.missing_bp").unwrap().state,
            "unavailable"
        );

        // Trace records blueprint_inputs when present.
        let traces = crate::trace::Traces::default();
        let rec = traces.start("automation", "sec1", expanded, Context::new(None));
        rec.set_blueprint_inputs(Some(raw.clone()));
        rec.finish("finished", None);
        let ext = traces.get("automation", "sec1", &rec.run_id()).unwrap();
        assert_eq!(ext["blueprint_inputs"], raw);

        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test(start_paused = true)]
    async fn motion_light_blueprint_runs_end_to_end() {
        let dir = crate::e2e_tests::fixture_dir();
        let cfg = dir.join("config");
        let bp_dir = cfg.join("blueprints/automation/homeassistant");
        std::fs::create_dir_all(&bp_dir).unwrap();
        std::fs::write(bp_dir.join("motion_light.yaml"), MOTION_LIGHT_YAML).unwrap();
        std::fs::write(
            cfg.join("configuration.yaml"),
            "homeassistant:\n  time_zone: UTC\nautomation: !include automations.yaml\n",
        )
        .unwrap();
        std::fs::write(
            cfg.join("automations.yaml"),
            r#"- id: "bp_motion_light_default"
  alias: "Motion Light (default wait)"
  use_blueprint:
    path: homeassistant/motion_light.yaml
    input:
      motion_entity: binary_sensor.hall_motion
      light_target:
        entity_id: light.hall

- id: "bp_motion_light_zero"
  alias: "Motion Light (zero wait)"
  use_blueprint:
    path: homeassistant/motion_light.yaml
    input:
      motion_entity: binary_sensor.kitchen_motion
      light_target:
        entity_id: light.kitchen
      no_motion_wait: 0
"#,
        )
        .unwrap();

        let app =
            std::sync::Arc::new(crate::App::load(&cfg, &dir.join("frontend"), false).unwrap());
        crate::automation::start(&app);

        app.states.set(
            "binary_sensor.kitchen_motion",
            "off",
            json!({"device_class": "motion"})
                .as_object()
                .unwrap()
                .clone(),
            Context::new(None),
        );
        app.states
            .set("light.kitchen", "off", Map::new(), Context::new(None));
        tokio::task::yield_now().await;

        app.states.set(
            "binary_sensor.kitchen_motion",
            "on",
            json!({"device_class": "motion"})
                .as_object()
                .unwrap()
                .clone(),
            Context::new(None),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(app.states.get("light.kitchen").unwrap().state, "on");

        app.states.set(
            "binary_sensor.kitchen_motion",
            "off",
            json!({"device_class": "motion"})
                .as_object()
                .unwrap()
                .clone(),
            Context::new(None),
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(app.states.get("light.kitchen").unwrap().state, "off");

        let fixture: Value = serde_json::from_str(include_str!(
            "../../../compat/fixtures/step2/pr4-blueprints.json"
        ))
        .unwrap();
        let expected_trace =
            &fixture["traces"]["automation.bp_motion_light_zero"][0]["extended_dict"];
        let runs = app.traces.list("automation", Some("bp_motion_light_zero"));
        assert_eq!(runs.len(), 1);
        let run_id = runs[0]["run_id"].as_str().unwrap();
        let ext = app
            .traces
            .get("automation", "bp_motion_light_zero", run_id)
            .unwrap();
        assert_eq!(ext["last_step"], expected_trace["last_step"]);
        assert_eq!(ext["script_execution"], expected_trace["script_execution"]);
        assert_eq!(ext["config"], expected_trace["config"]);
        assert_eq!(ext["blueprint_inputs"], expected_trace["blueprint_inputs"]);

        crate::automation::stop(&app).await;
        std::fs::remove_dir_all(dir).ok();
    }
}
