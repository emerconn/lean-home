//! MQTT with HA discovery (roadmap step 1). Reads the broker settings from HA's own config entry,
//! so an HA config dir with MQTT set up connects unchanged.

mod abbreviations;
pub mod entity;

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use indexmap::IndexMap;
use rumqttc::{AsyncClient, Event, LastWill, MqttOptions, Outgoing, Packet, QoS};
use serde_json::{Map, Value};

use crate::App;
use crate::state::Context;
use entity::{Entity, Publish};

const COMPONENTS: [&str; 4] = ["binary_sensor", "light", "sensor", "switch"];

#[derive(Debug)]
pub struct Settings {
    broker: String,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    client_id: Option<String>,
    keepalive: u64,
    discovery: bool,
    prefix: String,
    birth: Option<Msg>,
    will: Option<Msg>,
}

#[derive(Debug, Clone)]
struct Msg {
    topic: String,
    payload: String,
    qos: u8,
    retain: bool,
}

/// The `mqtt` entry in `.storage/core.config_entries`, if any and enabled.
pub fn settings(config_dir: &Path) -> Result<Option<Settings>, String> {
    let path = config_dir.join(".storage/core.config_entries");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let store: Value =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(entry) = store["data"]["entries"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|e| e["domain"] == "mqtt" && e["disabled_by"].is_null())
    else {
        return Ok(None);
    };
    // HA moved most settings to options; read both, options winning.
    let mut conf = entry["data"].as_object().cloned().unwrap_or_default();
    conf.extend(entry["options"].as_object().cloned().unwrap_or_default());
    let s = |k: &str| conf.get(k).and_then(Value::as_str).map(String::from);
    if conf
        .get("transport")
        .and_then(Value::as_str)
        .is_some_and(|t| t != "tcp")
        || conf.contains_key("certificate")
    {
        // shortcut: plain TCP only; TLS and websockets come with rumqttc's features when needed.
        return Err("mqtt: only plain TCP brokers are supported yet (no TLS/websockets)".into());
    }
    // HA's defaults for absent birth/will; an empty dict disables them.
    let msg = |k: &str, payload: &str| -> Option<Msg> {
        match conf.get(k) {
            None => Some(Msg {
                topic: "homeassistant/status".into(),
                payload: payload.into(),
                qos: 0,
                retain: false,
            }),
            Some(Value::Object(m)) if !m.is_empty() => Some(Msg {
                topic: m.get("topic")?.as_str()?.into(),
                payload: m
                    .get("payload")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .into(),
                qos: m.get("qos").and_then(Value::as_u64).unwrap_or(0).min(2) as u8,
                retain: m.get("retain").and_then(Value::as_bool).unwrap_or(false),
            }),
            _ => None,
        }
    };
    Ok(Some(Settings {
        broker: s("broker").ok_or("mqtt config entry has no broker")?,
        port: conf.get("port").and_then(Value::as_u64).unwrap_or(1883) as u16,
        username: s("username").filter(|u| !u.is_empty()),
        password: s("password"),
        client_id: s("client_id").filter(|c| !c.is_empty()),
        keepalive: conf.get("keepalive").and_then(Value::as_u64).unwrap_or(60),
        discovery: conf
            .get("discovery")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        prefix: s("discovery_prefix").unwrap_or_else(|| "homeassistant".into()),
        birth: msg("birth_message", "online"),
        will: msg("will_message", "offline"),
    }))
}

fn qos(q: u8) -> QoS {
    match q {
        2 => QoS::ExactlyOnce,
        1 => QoS::AtLeastOnce,
        _ => QoS::AtMostOnce,
    }
}

pub struct Mqtt {
    client: AsyncClient,
    settings: Settings,
    /// (component, discovery id) -> entity
    entities: Mutex<IndexMap<(String, String), Entity>>,
    /// Signalled once the DISCONNECT has gone out.
    stopped: tokio::sync::Notify,
}

impl Mqtt {
    /// Connects in the background; reconnects on its own.
    pub fn start(app: Arc<App>, settings: Settings) -> Arc<Self> {
        let id = settings
            .client_id
            .clone()
            .unwrap_or_else(|| format!("lean-home-{}", crate::auth::random_hex(6)));
        let mut opts = MqttOptions::new(id, settings.broker.clone(), settings.port);
        opts.set_keep_alive(Duration::from_secs(settings.keepalive.max(5)));
        if let Some(u) = &settings.username {
            opts.set_credentials(u.clone(), settings.password.clone().unwrap_or_default());
        }
        if let Some(w) = &settings.will {
            opts.set_last_will(LastWill::new(
                &w.topic,
                w.payload.clone(),
                qos(w.qos),
                w.retain,
            ));
        }
        // shortcut: MQTT 3.1.1 (HA defaults new entries to 5); every broker HA supports speaks it.
        // Requests are queued without awaiting, so the queue must hold a full resubscribe.
        let (client, mut events) = AsyncClient::new(opts, 4096);
        let mqtt = Arc::new(Self {
            client,
            settings,
            entities: Mutex::default(),
            stopped: tokio::sync::Notify::new(),
        });
        let m = mqtt.clone();
        tokio::spawn(async move {
            loop {
                match events.poll().await {
                    Ok(Event::Incoming(Packet::ConnAck(_))) => m.on_connect(),
                    Ok(Event::Incoming(Packet::Publish(p))) => {
                        match std::str::from_utf8(&p.payload) {
                            Ok(payload) => m.on_message(&app, &p.topic, payload),
                            Err(_) => eprintln!("mqtt: ignoring non-UTF-8 payload on {}", p.topic),
                        }
                    }
                    Ok(Event::Outgoing(Outgoing::Disconnect)) => return m.stopped.notify_one(),
                    Ok(_) => {}
                    Err(e) => {
                        eprintln!(
                            "mqtt {}:{}: {e}; retrying in 5 s",
                            m.settings.broker, m.settings.port
                        );
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
        });
        mqtt
    }

    /// Clean shutdown, as HA does it: publish the will message ourselves, then disconnect (a
    /// clean disconnect means the broker won't send the will).
    pub async fn stop(&self) {
        if let Some(w) = &self.settings.will {
            let _ = self
                .client
                .publish(&w.topic, qos(w.qos), w.retain, w.payload.clone())
                .await;
        }
        if self.client.disconnect().await.is_ok() {
            let _ = tokio::time::timeout(Duration::from_secs(2), self.stopped.notified()).await;
        }
    }

    fn on_connect(&self) {
        eprintln!(
            "mqtt: connected to {}:{}",
            self.settings.broker, self.settings.port
        );
        if self.settings.discovery {
            let p = &self.settings.prefix;
            for c in COMPONENTS {
                self.subscribe(&format!("{p}/{c}/+/config"), 0);
                self.subscribe(&format!("{p}/{c}/+/+/config"), 0);
            }
        }
        for e in self.entities.lock().unwrap().values() {
            for t in e.topics() {
                self.subscribe(&t, entity_qos(e));
            }
        }
        if let Some(b) = &self.settings.birth {
            self.publish(&Publish {
                topic: b.topic.clone(),
                payload: b.payload.clone(),
                qos: b.qos,
                retain: b.retain,
            });
        }
    }

    fn subscribe(&self, topic: &str, q: u8) {
        if let Err(e) = self.client.try_subscribe(topic, qos(q)) {
            eprintln!("mqtt: subscribe {topic}: {e}");
        }
    }

    fn publish(&self, p: &Publish) {
        if let Err(e) = self
            .client
            .try_publish(&p.topic, qos(p.qos), p.retain, p.payload.clone())
        {
            eprintln!("mqtt: publish {}: {e}", p.topic);
        }
    }

    fn on_message(&self, app: &App, topic: &str, payload: &str) {
        if let Some(rest) = topic.strip_prefix(&format!("{}/", self.settings.prefix))
            && let Some((component, id)) = parse_discovery_topic(rest)
        {
            return self.discover(app, component, &id, payload);
        }
        let mut entities = self.entities.lock().unwrap();
        for e in entities.values_mut() {
            if e.topics().iter().any(|t| topic_matches(t, topic)) && e.on_message(topic, payload) {
                write_state(app, e);
            }
        }
    }

    fn discover(&self, app: &App, component: &str, id: &str, payload: &str) {
        let key = (component.to_string(), id.to_string());
        let mut entities = self.entities.lock().unwrap();
        // An empty payload removes the entity.
        if payload.is_empty() {
            if let Some(e) = entities.shift_remove(&key) {
                app.states.remove(&e.entity_id);
            }
            return;
        }
        let cfg = match serde_json::from_str::<Value>(payload) {
            Ok(Value::Object(mut cfg)) => {
                expand(&mut cfg);
                cfg
            }
            _ => return eprintln!("mqtt: invalid discovery payload on {component}/{id}"),
        };
        let mut entity = match Entity::new(component, cfg) {
            Ok(e) => e,
            Err(e) => return eprintln!("mqtt: {component}/{id}: {e}"),
        };
        let reg = app
            .registry
            .mqtt_entity(entity.kind.domain(), entity.unique_id());
        if reg.is_some_and(|r| !r["disabled_by"].is_null()) {
            return; // HA creates no state for disabled entities
        }
        let device = entity.device().and_then(|d| app.registry.mqtt_device(d));
        let device_name = device
            .and_then(|d| d["name_by_user"].as_str().or(d["name"].as_str()))
            .map(String::from)
            .or_else(|| {
                entity
                    .device()
                    .and_then(|d| d.get("name"))
                    .and_then(Value::as_str)
                    .map(String::from)
            });
        let name = reg
            .and_then(|r| r["name"].as_str())
            .map(String::from)
            .or_else(|| entity.name());
        let friendly = match (&device_name, name) {
            (Some(d), Some(n)) => Some(format!("{d} {n}")),
            (Some(d), None) => Some(d.clone()),
            (None, n) => n,
        };
        entity.entity_id = match entities.get(&key) {
            Some(old) => old.entity_id.clone(), // an update keeps its entity_id
            None => match reg.and_then(|r| r["entity_id"].as_str()) {
                Some(id) => id.to_string(),
                None => new_entity_id(app, entity.kind.domain(), &entity, friendly.as_deref(), id),
            },
        };
        if let Some(f) = friendly {
            entity.cfg.insert("friendly_name".into(), f.into());
        }
        for t in entity.topics() {
            self.subscribe(&t, entity_qos(&entity));
        }
        write_state(app, &entity);
        entities.insert(key, entity);
    }

    /// A service call on one of our entities; `false` if the entity isn't ours.
    pub fn call(
        &self,
        app: &App,
        entity_id: &str,
        service: &str,
        data: &Value,
        ctx: &Context,
    ) -> bool {
        let mut entities = self.entities.lock().unwrap();
        let Some(e) = entities.values_mut().find(|e| e.entity_id == entity_id) else {
            return false;
        };
        for p in e.command(service, data, ctx) {
            self.publish(&p);
        }
        write_state(app, e);
        true
    }
}

fn entity_qos(e: &Entity) -> u8 {
    e.cfg.get("qos").and_then(Value::as_u64).unwrap_or(0).min(2) as u8
}

fn write_state(app: &App, e: &Entity) {
    let (state, attrs) = e.state();
    app.states.set(&e.entity_id, &state, attrs, e.context());
}

/// HA: `default_entity_id` if given, else the slugified friendly name; `_2`, `_3`… on collision.
// shortcut: new entities aren't written to core.entity_registry, so HA picks its own id for them
// later (normally the same one). Write the registry when lean-home manages entities itself.
fn new_entity_id(
    app: &App,
    domain: &str,
    e: &Entity,
    friendly: Option<&str>,
    discovery_id: &str,
) -> String {
    let object_id = e
        .cfg
        .get("default_entity_id")
        .or_else(|| e.cfg.get("object_id"))
        .and_then(Value::as_str)
        .map(|d| d.rsplit('.').next().unwrap_or(d).to_string())
        .unwrap_or_else(|| slugify(friendly.unwrap_or(discovery_id)));
    let base = format!("{domain}.{object_id}");
    let mut id = base.clone();
    let mut n = 2;
    while app.states.get(&id).is_some() || app.registry.entity_id_taken(&id) {
        id = format!("{base}_{n}");
        n += 1;
    }
    id
}

/// HA's `slugify`, minus transliteration.
// shortcut: non-ASCII letters become "_" where HA transliterates ("Küche" -> "kuche").
pub fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let out = out.trim_matches('_');
    if out.is_empty() {
        "unknown".into()
    } else {
        out.into()
    }
}

/// `<component>/[<node_id>/]<object_id>/config` -> (component, discovery id), as HA's TOPIC_MATCHER.
fn parse_discovery_topic(rest: &str) -> Option<(&str, String)> {
    let parts: Vec<_> = rest.split('/').collect();
    let ok = |s: &str| {
        !s.is_empty()
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    };
    match parts.as_slice() {
        [c, obj, "config"] if COMPONENTS.contains(c) && ok(obj) => Some((c, obj.to_string())),
        [c, node, obj, "config"] if COMPONENTS.contains(c) && ok(node) && ok(obj) => {
            Some((c, format!("{node} {obj}")))
        }
        _ => None,
    }
}

/// MQTT topic filter matching (`+`, `#`).
fn topic_matches(filter: &str, topic: &str) -> bool {
    let (mut f, mut t) = (filter.split('/'), topic.split('/'));
    loop {
        match (f.next(), t.next()) {
            (Some("#"), _) => return true,
            (Some("+"), Some(_)) => {}
            (Some(a), Some(b)) if a == b => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

fn expand_with(m: &mut Map<String, Value>, table: &[(&str, &str)]) {
    let keys: Vec<String> = m.keys().cloned().collect();
    for k in keys {
        if let Ok(i) = table.binary_search_by(|(a, _)| (*a).cmp(k.as_str())) {
            let v = m.shift_remove(&k).unwrap();
            m.insert(table[i].1.into(), v);
        }
    }
}

/// HA's `_replace_all_abbreviations` + `_replace_topic_base` for a component payload.
fn expand(cfg: &mut Map<String, Value>) {
    expand_with(cfg, abbreviations::ABBREVIATIONS);
    for a in cfg
        .get_mut("availability")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(a) = a.as_object_mut() {
            expand_with(a, abbreviations::ABBREVIATIONS);
        }
    }
    if let Some(o) = cfg.get_mut("origin").and_then(Value::as_object_mut) {
        expand_with(o, abbreviations::ORIGIN_ABBREVIATIONS);
    }
    if let Some(d) = cfg.get_mut("device").and_then(Value::as_object_mut) {
        expand_with(d, abbreviations::DEVICE_ABBREVIATIONS);
    }
    let Some(Value::String(base)) = cfg.shift_remove("~") else {
        return;
    };
    let fix = |v: &mut Value| {
        if let Value::String(s) = v {
            if let Some(rest) = s.strip_prefix('~') {
                *s = format!("{base}{rest}");
            }
            if let Some(rest) = s.strip_suffix('~') {
                *s = format!("{rest}{base}");
            }
        }
    };
    for (k, v) in cfg.iter_mut() {
        if k.ends_with("topic") {
            fix(v);
        }
    }
    for a in cfg
        .get_mut("availability")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        if let Some(t) = a.get_mut("topic") {
            fix(t);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn abbreviations_and_topic_base() {
        let mut cfg = json!({"~": "z2m/lamp", "stat_t": "~/state", "cmd_t": "~/set", "avty": [{"t": "~/avail"}],
            "dev": {"ids": ["x"], "mf": "acme"}, "uniq_id": "u1"})
        .as_object()
        .unwrap()
        .clone();
        expand(&mut cfg);
        assert_eq!(
            Value::Object(cfg),
            json!({"state_topic": "z2m/lamp/state", "command_topic": "z2m/lamp/set", "availability": [{"topic": "z2m/lamp/avail"}],
                   "device": {"identifiers": ["x"], "manufacturer": "acme"}, "unique_id": "u1"})
        );
        assert!(
            abbreviations::ABBREVIATIONS
                .windows(2)
                .all(|w| w[0].0 < w[1].0),
            "table must stay sorted"
        );
    }

    #[test]
    fn topics() {
        assert_eq!(
            parse_discovery_topic("light/node-1/obj_2/config"),
            Some(("light", "node-1 obj_2".into()))
        );
        assert_eq!(
            parse_discovery_topic("light/obj/config"),
            Some(("light", "obj".into()))
        );
        assert_eq!(parse_discovery_topic("light/ob j/config"), None);
        assert_eq!(parse_discovery_topic("climate/x/config"), None);
        assert!(
            topic_matches("a/+/c", "a/b/c")
                && topic_matches("a/#", "a/b/c")
                && !topic_matches("a/b", "a/b/c")
        );
        assert_eq!(slugify("Demo Device  Light!"), "demo_device_light");
    }
}
