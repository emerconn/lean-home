//! One MQTT-discovered entity: its config, the topics it listens to, its state and its commands.
//! Behaviour follows HA 2026.11.2 `components/mqtt/{switch,binary_sensor,sensor,light/*}.py`.

use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::state::Context;

const PAYLOAD_NONE: &str = "None";
/// HA attributes state writes to the context of a service call made this recently.
const CONTEXT_RECENT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Switch,
    BinarySensor,
    Sensor,
    Light,
    JsonLight,
}

impl Kind {
    pub fn domain(self) -> &'static str {
        match self {
            Kind::Switch => "switch",
            Kind::BinarySensor => "binary_sensor",
            Kind::Sensor => "sensor",
            Kind::Light | Kind::JsonLight => "light",
        }
    }

    fn default_name(self) -> &'static str {
        match self {
            Kind::Switch => "MQTT Switch",
            Kind::BinarySensor => "MQTT Binary sensor",
            Kind::Sensor => "MQTT Sensor",
            Kind::Light => "MQTT LightEntity",
            Kind::JsonLight => "MQTT JSON Light",
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Publish {
    pub topic: String,
    pub payload: String,
    pub qos: u8,
    pub retain: bool,
}

#[derive(Debug)]
struct Avail {
    topic: String,
    on: String,
    off: String,
    template: Option<String>,
    value: Option<bool>,
}

#[derive(Debug)]
pub struct Entity {
    pub kind: Kind,
    pub cfg: Map<String, Value>,
    pub entity_id: String,
    is_on: Option<bool>,
    brightness: Option<u8>,
    value: Option<String>,
    avail: Vec<Avail>,
    /// For availability_mode "latest".
    last_avail: Option<bool>,
    context: Option<(Context, Instant)>,
    /// The device-based discovery message this entity came from, if any.
    pub device_discovery: Option<String>,
}

impl Entity {
    /// `cfg` is a discovery payload with abbreviations and `~` already expanded.
    pub fn new(component: &str, cfg: Map<String, Value>) -> Result<Self, String> {
        let kind = match component {
            "switch" => Kind::Switch,
            "binary_sensor" => Kind::BinarySensor,
            "sensor" => Kind::Sensor,
            "light" => match cfg.get("schema").and_then(Value::as_str).unwrap_or("basic") {
                "basic" | "default" => Kind::Light,
                "json" => Kind::JsonLight,
                // shortcut: the "template" light schema is rare; add when a device needs it.
                s => return Err(format!("light schema {s} not supported yet")),
            },
            c => return Err(format!("component {c} not supported yet")),
        };
        let mut avail = vec![];
        let s = |m: &Map<String, Value>, k: &str, d: &str| {
            m.get(k).and_then(Value::as_str).unwrap_or(d).to_string()
        };
        if let Some(topic) = cfg.get("availability_topic").and_then(Value::as_str) {
            avail.push(Avail {
                topic: topic.into(),
                on: s(&cfg, "payload_available", "online"),
                off: s(&cfg, "payload_not_available", "offline"),
                template: cfg
                    .get("availability_template")
                    .and_then(Value::as_str)
                    .map(String::from),
                value: None,
            });
        }
        for a in cfg
            .get("availability")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(a) = a.as_object() else { continue };
            let Some(topic) = a.get("topic").and_then(Value::as_str) else {
                continue;
            };
            avail.push(Avail {
                topic: topic.into(),
                on: s(a, "payload_available", "online"),
                off: s(a, "payload_not_available", "offline"),
                template: a
                    .get("value_template")
                    .and_then(Value::as_str)
                    .map(String::from),
                value: None,
            });
        }
        if matches!(kind, Kind::Switch | Kind::Light | Kind::JsonLight)
            && !cfg.contains_key("command_topic")
        {
            return Err("command_topic is required".into());
        }
        if matches!(kind, Kind::BinarySensor | Kind::Sensor) && !cfg.contains_key("state_topic") {
            return Err("state_topic is required".into());
        }
        Ok(Self {
            kind,
            cfg,
            entity_id: String::new(),
            is_on: None,
            brightness: None,
            value: None,
            avail,
            last_avail: None,
            context: None,
            device_discovery: None,
        })
    }

    fn str(&self, key: &str) -> Option<&str> {
        self.cfg.get(key).and_then(Value::as_str)
    }

    fn payload_on(&self) -> &str {
        self.str("payload_on").unwrap_or("ON")
    }

    fn payload_off(&self) -> &str {
        self.str("payload_off").unwrap_or("OFF")
    }

    fn brightness_scale(&self) -> f64 {
        self.cfg
            .get("brightness_scale")
            .and_then(Value::as_f64)
            .filter(|s| *s >= 1.0)
            .unwrap_or(255.0)
    }

    pub fn unique_id(&self) -> Option<&str> {
        self.str("unique_id")
    }

    pub fn device(&self) -> Option<&Map<String, Value>> {
        self.cfg.get("device").and_then(Value::as_object)
    }

    /// HA's optimistic mode: no state topic, or forced by config.
    fn optimistic(&self) -> bool {
        self.cfg
            .get("optimistic")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || !self.cfg.contains_key("state_topic")
    }

    /// The entity's own name: `Some(None)` = explicitly null (the device's main feature).
    pub fn name(&self) -> Option<String> {
        match self.cfg.get("name") {
            Some(Value::Null) => None,
            Some(v) => v.as_str().map(String::from),
            None => {
                let dc = self.str("device_class").filter(|d| *d != "enum");
                match (self.kind, dc) {
                    // HA uses the translated device-class name here.
                    // shortcut: English from the key ("carbon_dioxide" -> "Carbon dioxide"); use the
                    // translations once they are bundled.
                    (Kind::Sensor | Kind::BinarySensor, Some(dc)) => {
                        let mut s = dc.replace('_', " ");
                        s[..1].make_ascii_uppercase();
                        Some(s)
                    }
                    _ => Some(self.kind.default_name().into()),
                }
            }
        }
    }

    /// Every topic this entity listens to.
    pub fn topics(&self) -> Vec<String> {
        let mut t: Vec<String> = ["state_topic", "brightness_state_topic"]
            .iter()
            .filter_map(|k| self.str(k).map(String::from))
            .collect();
        t.extend(self.avail.iter().map(|a| a.topic.clone()));
        t.dedup();
        t
    }

    pub fn available(&self) -> bool {
        if self.avail.is_empty() {
            return true;
        }
        match self.str("availability_mode").unwrap_or("latest") {
            "all" => self.avail.iter().all(|a| a.value == Some(true)),
            "any" => self.avail.iter().any(|a| a.value == Some(true)),
            _ => self.last_avail == Some(true),
        }
    }

    /// Apply a message; returns whether the entity's state may have changed.
    pub fn on_message(&mut self, topic: &str, payload: &str) -> bool {
        let mut changed = false;
        for i in 0..self.avail.len() {
            if self.avail[i].topic != topic {
                continue;
            }
            let Some(p) = self.render(self.avail[i].template.as_deref(), payload) else {
                continue;
            };
            let a = &mut self.avail[i];
            let v = if p == a.on {
                true
            } else if p == a.off {
                false
            } else {
                continue;
            };
            a.value = Some(v);
            self.last_avail = Some(v);
            changed = true;
        }
        if self.str("state_topic") == Some(topic) {
            changed |= self.on_state(payload);
        }
        if self.kind == Kind::Light && self.str("brightness_state_topic") == Some(topic) {
            let tpl = self.str("brightness_value_template").map(String::from);
            if let Some(v) = self
                .render(tpl.as_deref(), payload)
                .and_then(|p| p.trim().parse::<f64>().ok())
            {
                // HA ignores empty and zero brightness.
                if v != 0.0 {
                    self.brightness = Some(to_ha_brightness(v, self.brightness_scale()));
                    changed = true;
                }
            }
        }
        changed
    }

    fn on_state(&mut self, payload: &str) -> bool {
        match self.kind {
            Kind::Switch | Kind::BinarySensor => {
                let tpl = self.str("value_template").map(String::from);
                let Some(p) = self.render(tpl.as_deref(), payload) else {
                    return false;
                };
                let on = self
                    .str("state_on")
                    .unwrap_or(self.payload_on())
                    .to_string();
                let off = self
                    .str("state_off")
                    .unwrap_or(self.payload_off())
                    .to_string();
                self.is_on = match p.as_str() {
                    p if p == on => Some(true),
                    p if p == off => Some(false),
                    PAYLOAD_NONE => None,
                    _ => return false,
                };
            }
            Kind::Sensor => {
                let tpl = self.str("value_template").map(String::from);
                let Some(p) = self.render(tpl.as_deref(), payload) else {
                    return false;
                };
                // shortcut: no numeric validation or rounding for sensors with a unit/state_class
                // (HA rejects non-numeric values there); add with the sensor precision options.
                self.value = (p != PAYLOAD_NONE).then_some(p);
            }
            Kind::Light => {
                let tpl = self.str("state_value_template").map(String::from);
                let Some(p) = self
                    .render(tpl.as_deref(), payload)
                    .filter(|p| !p.is_empty())
                else {
                    return false;
                };
                self.is_on = match p.as_str() {
                    p if p == self.payload_on() => Some(true),
                    p if p == self.payload_off() => Some(false),
                    PAYLOAD_NONE => None,
                    _ => return false,
                };
            }
            Kind::JsonLight => {
                let Ok(Value::Object(v)) = serde_json::from_str::<Value>(payload) else {
                    return false;
                };
                match v.get("state") {
                    Some(Value::String(s)) if s == "ON" => self.is_on = Some(true),
                    Some(Value::String(s)) if s == "OFF" => self.is_on = Some(false),
                    Some(Value::Null) => self.is_on = None,
                    _ => {}
                }
                if self.supports_brightness()
                    && let Some(b) = v
                        .get("brightness")
                        .and_then(Value::as_f64)
                        .filter(|b| *b != 0.0)
                {
                    self.brightness = Some(to_ha_brightness(b, self.brightness_scale()));
                }
            }
        }
        true
    }

    /// Render a value template with `value`/`value_json`, as HA's MqttValueTemplate does.
    /// `None` when rendering fails (HA logs the error and ignores the message).
    fn render(&self, template: Option<&str>, payload: &str) -> Option<String> {
        let Some(tpl) = template else {
            return Some(payload.to_string());
        };
        let value_json: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
        let ctx = minijinja::context! {
            value => payload,
            value_json => minijinja::value::Serde(&value_json),
            entity_id => &self.entity_id,
        };
        // shortcut: parsed on every message and without HA's template filters/globals; cache the
        // compiled template and add HA's helpers with roadmap step 2.
        match minijinja::Environment::new().render_str(tpl, ctx) {
            Ok(s) => Some(s.trim().to_string()),
            Err(e) => {
                eprintln!("mqtt {}: template error: {e}", self.entity_id);
                None
            }
        }
    }

    fn supports_brightness(&self) -> bool {
        match self.kind {
            Kind::Light => self.cfg.contains_key("brightness_command_topic"),
            Kind::JsonLight => {
                self.cfg
                    .get("brightness")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    || self
                        .cfg
                        .get("supported_color_modes")
                        .and_then(Value::as_array)
                        .is_some_and(|m| m.iter().any(|m| m == "brightness"))
            }
            _ => false,
        }
    }

    /// The state and attributes to write to the state machine.
    pub fn state(&self) -> (String, Map<String, Value>) {
        let mut a = Map::new();
        let available = self.available();
        let light = matches!(self.kind, Kind::Light | Kind::JsonLight);
        if light {
            let mode = if self.supports_brightness() {
                "brightness"
            } else {
                "onoff"
            };
            a.insert("supported_color_modes".into(), json!([mode]));
            if available {
                let on = self.is_on == Some(true);
                a.insert(
                    "color_mode".into(),
                    if on { json!(mode) } else { Value::Null },
                );
                if mode == "brightness" {
                    a.insert(
                        "brightness".into(),
                        if on {
                            json!(self.brightness)
                        } else {
                            Value::Null
                        },
                    );
                }
            }
        }
        // HA's order (helpers/entity.py): capabilities, state attributes, unit, assumed_state,
        // device_class, icon, friendly_name, supported_features.
        let cfg = |k: &str| self.cfg.get(k).filter(|v| !v.is_null()).cloned();
        if self.kind == Kind::Sensor {
            if let Some(v) = cfg("state_class") {
                a.insert("state_class".into(), v);
            }
            if let Some(v) = cfg("unit_of_measurement") {
                a.insert("unit_of_measurement".into(), v);
            }
        }
        if matches!(self.kind, Kind::Switch | Kind::Light | Kind::JsonLight) && self.optimistic() {
            a.insert("assumed_state".into(), true.into());
        }
        if !light && let Some(dc) = cfg("device_class") {
            a.insert("device_class".into(), dc);
        }
        if let Some(icon) = cfg("icon") {
            a.insert("icon".into(), icon);
        }
        if let Some(f) = cfg("friendly_name") {
            a.insert("friendly_name".into(), f);
        }
        match self.kind {
            Kind::Light => a.insert("supported_features".into(), 0.into()),
            // HA's JSON schema always reports flash + transition.
            Kind::JsonLight => a.insert("supported_features".into(), 40.into()),
            _ => None,
        };
        let state = if !available {
            "unavailable".to_string()
        } else {
            match self.kind {
                Kind::Sensor => self.value.clone().unwrap_or_else(|| "unknown".into()),
                _ => match self.is_on {
                    Some(true) => "on".into(),
                    Some(false) => "off".into(),
                    None => "unknown".into(),
                },
            }
        };
        (state, a)
    }

    /// Context for the next state write: a recent service call's, else a fresh one.
    pub fn context(&self) -> Context {
        match &self.context {
            Some((c, at)) if at.elapsed() < CONTEXT_RECENT => c.clone(),
            _ => Context::new(None),
        }
    }

    /// `turn_on`/`turn_off`/`toggle`. Returns the messages to publish; optimistic entities also
    /// update their own state.
    pub fn command(&mut self, service: &str, data: &Value, ctx: &Context) -> Vec<Publish> {
        self.context = Some((ctx.clone(), Instant::now()));
        let mut brightness = data
            .get("brightness")
            .and_then(Value::as_f64)
            .map(|b| b.clamp(0.0, 255.0));
        if let Some(pct) = data.get("brightness_pct").and_then(Value::as_f64) {
            brightness = Some((pct.clamp(0.0, 100.0) * 255.0 / 100.0).round_ties_even());
        }
        let on = match service {
            "turn_on" => brightness != Some(0.0), // HA: turn_on with brightness 0 turns off
            "turn_off" => false,
            "toggle" => self.is_on != Some(true),
            _ => return vec![],
        };
        let brightness = brightness.filter(|_| on && self.supports_brightness());
        let qos = self
            .cfg
            .get("qos")
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(2) as u8;
        let retain = self
            .cfg
            .get("retain")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let msg = |topic: &str, payload: String| Publish {
            topic: topic.into(),
            payload,
            qos,
            retain,
        };
        let cmd_topic = self.str("command_topic").unwrap_or_default().to_string();
        let mut out = vec![];
        match self.kind {
            Kind::Switch => {
                let p = if on {
                    self.payload_on()
                } else {
                    self.payload_off()
                }
                .to_string();
                out.push(msg(
                    &cmd_topic,
                    self.command_template("command_template", &p),
                ));
            }
            Kind::Light => {
                let on_payload = || msg(&cmd_topic, self.payload_on().to_string());
                if !on {
                    out.push(msg(&cmd_topic, self.payload_off().to_string()));
                } else {
                    let on_type = self.str("on_command_type").unwrap_or("last");
                    if on_type == "first" {
                        out.push(on_payload());
                    }
                    let b = brightness.or_else(|| {
                        (on_type == "brightness").then(|| self.brightness.map_or(255.0, f64::from))
                    });
                    if let (Some(b), Some(topic)) = (b, self.str("brightness_command_topic")) {
                        let scale = self.brightness_scale();
                        let device =
                            ((b / 255.0 * scale).round_ties_even()).min(scale).max(1.0) as u64;
                        let topic = topic.to_string();
                        out.push(msg(
                            &topic,
                            self.command_template(
                                "brightness_command_template",
                                &device.to_string(),
                            ),
                        ));
                    }
                    if on_type == "last" {
                        out.push(on_payload());
                    }
                }
            }
            Kind::JsonLight => {
                let mut m = json!({"state": if on { "ON" } else { "OFF" }});
                if let Some(b) = brightness {
                    m["brightness"] = json!(
                        (b * self.brightness_scale() / 255.0)
                            .round_ties_even()
                            .max(1.0) as u64
                    );
                }
                out.push(msg(&cmd_topic, m.to_string()));
            }
            Kind::BinarySensor | Kind::Sensor => return vec![],
        }
        if self.optimistic() {
            self.is_on = Some(on);
            if let Some(b) = brightness {
                self.brightness = Some(b as u8);
            }
        }
        out
    }

    fn command_template(&self, key: &str, value: &str) -> String {
        let Some(tpl) = self.str(key) else {
            return value.to_string();
        };
        let ctx = minijinja::context! { value => value, entity_id => &self.entity_id };
        minijinja::Environment::new()
            .render_str(tpl, ctx)
            .unwrap_or_else(|e| {
                eprintln!("mqtt {}: command template error: {e}", self.entity_id);
                value.to_string()
            })
    }
}

/// A device brightness on `1..=scale` to HA's `1..=255` (HA's `value_to_brightness`).
fn to_ha_brightness(v: f64, scale: f64) -> u8 {
    (v * 255.0 / scale).round_ties_even().clamp(1.0, 255.0) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entity(component: &str, cfg: Value) -> Entity {
        let mut e = Entity::new(component, cfg.as_object().unwrap().clone()).unwrap();
        e.entity_id = format!("{component}.test");
        e
    }

    #[test]
    fn basic_light_brightness_and_order() {
        let mut l = entity(
            "light",
            json!({"command_topic": "l/set", "state_topic": "l/state", "brightness_command_topic": "l/b/set",
                   "brightness_state_topic": "l/b", "brightness_scale": 100}),
        );
        assert_eq!(l.state().0, "unknown");
        assert!(l.on_message("l/state", "ON"));
        assert!(l.on_message("l/b", "50"));
        let (s, a) = l.state();
        assert_eq!(
            (s.as_str(), &a["brightness"], &a["color_mode"]),
            ("on", &json!(128), &json!("brightness"))
        );
        // on_command_type "last": brightness first, then ON; not optimistic (has state_topic).
        let out = l.command(
            "turn_on",
            &json!({"brightness_pct": 50}),
            &Context::new(None),
        );
        assert_eq!(
            out.iter()
                .map(|p| (p.topic.as_str(), p.payload.as_str()))
                .collect::<Vec<_>>(),
            [("l/b/set", "50"), ("l/set", "ON")]
        );
        assert_eq!(
            l.command("toggle", &json!({}), &Context::new(None))[0].payload,
            "OFF"
        );
        assert!(l.on_message("l/state", "OFF"));
        let (s, a) = l.state();
        assert_eq!((s.as_str(), &a["brightness"]), ("off", &Value::Null));
    }

    #[test]
    fn json_light_and_availability() {
        let mut l = entity(
            "light",
            json!({"schema": "json", "command_topic": "j/set", "state_topic": "j", "brightness": true, "availability_topic": "j/avail"}),
        );
        assert_eq!(l.state().0, "unavailable");
        l.on_message("j/avail", "online");
        l.on_message("j", r#"{"state":"ON","brightness":255}"#);
        assert_eq!(l.state().1["brightness"], 255);
        let out = l.command("turn_on", &json!({"brightness": 128}), &Context::new(None));
        assert_eq!(out[0].payload, r#"{"state":"ON","brightness":128}"#);
        l.on_message("j/avail", "offline");
        let (s, a) = l.state();
        assert_eq!(s, "unavailable");
        assert!(a.get("brightness").is_none() && a.contains_key("supported_color_modes"));
    }

    #[test]
    fn switch_optimistic_and_sensor_template() {
        let mut s = entity("switch", json!({"command_topic": "s/set"}));
        let out = s.command("turn_on", &json!({}), &Context::new(None));
        assert_eq!(
            out,
            vec![Publish {
                topic: "s/set".into(),
                payload: "ON".into(),
                qos: 0,
                retain: false
            }]
        );
        let (st, a) = s.state();
        assert_eq!((st.as_str(), &a["assumed_state"]), ("on", &json!(true)));

        let mut t = entity(
            "sensor",
            json!({"state_topic": "t", "value_template": "{{ value_json.temperature }}", "device_class": "temperature"}),
        );
        t.on_message("t", r#"{"temperature": 21.5}"#);
        assert_eq!(t.state().0, "21.5");
        assert_eq!(t.name().as_deref(), Some("Temperature"));
        let mut raw = entity("sensor", json!({"state_topic": "r"}));
        raw.on_message("r", "abc");
        assert_eq!(raw.state().0, "abc");
        raw.on_message("r", "None");
        assert_eq!(raw.state().0, "unknown");

        let mut b = entity("binary_sensor", json!({"state_topic": "d"}));
        b.on_message("d", "ON");
        assert_eq!(b.state().0, "on");
        b.on_message("d", "garbage");
        assert_eq!(b.state().0, "on");
    }
}
