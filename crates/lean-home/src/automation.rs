//! Automations: HA's `automation` integration (`components/automation/`) and the triggers it
//! listens with (`state`, `mqtt` and `time`, from `components/homeassistant/triggers/` and
//! `components/mqtt/trigger.py`). An automation's entity is a `ScriptEntity` whose actions
//! `script.rs` runs; this module adds the triggers and conditions in front of a run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveTime, Timelike, Utc};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::AbortHandle;

use crate::condition::{Condition, Eval, WEEKDAYS, entity_ids, id_string, parse_weekdays};
use crate::mqtt::{slugify, topic_matches};
use crate::script::{Halt, ScriptEntity, Vars, launch, render_complex, time_period};
use crate::services::Outcome;
use crate::state::{Change, Context, Event, States};
use crate::{App, template};

/// One `triggers:` entry: HA's `TriggerData` (`id`, `idx`, `alias`) and the platform's config.
#[derive(Debug)]
struct Trigger {
    idx: usize,
    id: String,
    alias: Option<String>,
    enabled: Option<Value>,
    variables: Option<Map<String, Value>>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    State {
        entity_ids: Vec<String>,
        from: Match,
        to: Match,
        /// None of `from`/`to`/`not_from`/`not_to` given: attribute changes fire too.
        match_all: bool,
        attribute: Option<String>,
        for_: Option<Value>,
        /// `from` without `to`: a `for:` waits for the state to stay away from the old one.
        keep_from: bool,
    },
    Mqtt {
        topic: String,
        payload: Option<String>,
        value_template: Option<String>,
        qos: u8,
    },
    Time {
        at: Vec<NaiveTime>,
        weekdays: Option<Vec<String>>,
    },
    NumericState {
        entity_ids: Vec<String>,
        above: Option<Value>,
        below: Option<Value>,
        attribute: Option<String>,
        value_template: Option<String>,
        for_: Option<Value>,
    },
    Template {
        value_template: String,
        for_: Option<Value>,
    },
    HomeAssistant {
        event: HaEvent,
    },
    TimePattern {
        hours: Vec<u32>,
        minutes: Vec<u32>,
        seconds: Vec<u32>,
    },
    Event {
        event_types: Vec<String>,
        event_data: Option<Map<String, Value>>,
        context: Option<Map<String, Value>>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HaEvent {
    Start,
    Shutdown,
}

/// HA's `process_state_match`; `Null` is a missing state (the entity was added or removed).
#[derive(Debug)]
enum Match {
    Any,
    In(Vec<Value>),
    NotIn(Vec<Value>),
}

impl Match {
    fn test(&self, v: &Value) -> bool {
        match self {
            Self::Any => true,
            Self::In(l) => l.contains(v),
            Self::NotIn(l) => !l.contains(v),
        }
    }
}

/// One of `config`'s keys, or its legacy singular name (`triggers`/`trigger`).
fn plural<'a>(m: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    m.get(key).or_else(|| m.get(key.trim_end_matches('s')))
}

/// A list, or one item.
fn list(v: Option<&Value>) -> Vec<Value> {
    match v {
        Some(Value::Array(a)) => a.clone(),
        Some(Value::Null) | None => vec![],
        Some(one) => vec![one.clone()],
    }
}

fn parse_trigger(idx: usize, v: &Value) -> Result<Trigger, String> {
    let m = v.as_object().ok_or("trigger: expected a dictionary")?;
    let platform = m
        .get("trigger")
        .or_else(|| m.get("platform"))
        .and_then(Value::as_str)
        .ok_or("required key not provided @ data['trigger']")?;
    let str_opt = |k: &str| m.get(k).and_then(Value::as_str).map(String::from);
    let kind = match platform {
        "state" => {
            let attribute = str_opt("attribute");
            // Without `attribute` the states are strings: HA rejects `to: on` (a YAML bool).
            let matcher = |key: &str, not_key: &str| -> Result<(Match, bool), String> {
                let values = |v: &Value| -> Result<Option<Vec<Value>>, String> {
                    let ok = |x: &Value| attribute.is_some() || x.is_string();
                    match v {
                        Value::Null => Ok(None),
                        Value::Array(a) if a.iter().all(ok) => Ok(Some(a.clone())),
                        x if !x.is_array() && ok(x) => Ok(Some(vec![x.clone()])),
                        _ => Err(format!("expected str for dictionary value @ data['{key}']")),
                    }
                };
                Ok(match (m.get(key), m.get(not_key)) {
                    (Some(v), _) => (values(v)?.map_or(Match::Any, Match::In), true),
                    (None, Some(v)) => (values(v)?.map_or(Match::Any, Match::NotIn), true),
                    (None, None) => (Match::Any, false),
                })
            };
            let (from, has_from) = matcher("from", "not_from")?;
            let (to, has_to) = matcher("to", "not_to")?;
            let ids = entity_ids(m.get("entity_id").unwrap_or(&Value::Null));
            if ids.is_empty() {
                return Err("required key not provided @ data['entity_id']".into());
            }
            Kind::State {
                entity_ids: ids,
                from,
                to,
                match_all: !has_from && !has_to,
                attribute,
                for_: m.get("for").cloned(),
                keep_from: m.contains_key("from") && !m.contains_key("to"),
            }
        }
        "mqtt" => {
            let topic = str_opt("topic").ok_or("required key not provided @ data['topic']")?;
            if !template::is_static(&topic) {
                // shortcut: topic templates (rendered with trigger_variables) aren't supported.
                return Err("mqtt trigger: topic templates are not supported yet".into());
            }
            Kind::Mqtt {
                topic,
                payload: m.get("payload").map(id_string),
                value_template: str_opt("value_template"),
                qos: m.get("qos").and_then(Value::as_u64).unwrap_or(0).min(2) as u8,
            }
        }
        "time" => {
            let at = list(m.get("at"))
                .iter()
                .map(|a| {
                    // shortcut: only fixed times; `input_datetime`/timestamp-sensor entities,
                    // offsets and templates come when a config uses them.
                    a.as_str()
                        .and_then(|s| {
                            NaiveTime::parse_from_str(s, "%H:%M:%S%.f")
                                .or_else(|_| NaiveTime::parse_from_str(s, "%H:%M"))
                                .ok()
                        })
                        .ok_or_else(|| format!("time trigger: unsupported 'at' {a}"))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let weekdays = m.get("weekday").map(parse_weekdays).transpose()?;
            Kind::Time { at, weekdays }
        }
        "numeric_state" => {
            let ids = entity_ids(m.get("entity_id").unwrap_or(&Value::Null));
            if ids.is_empty() {
                return Err("required key not provided @ data['entity_id']".into());
            }
            if !m.contains_key("above") && !m.contains_key("below") {
                return Err(
                    "numeric_state trigger: must contain at least one of below, above".into(),
                );
            }
            Kind::NumericState {
                entity_ids: ids,
                above: m.get("above").cloned(),
                below: m.get("below").cloned(),
                attribute: str_opt("attribute"),
                value_template: str_opt("value_template"),
                for_: m.get("for").cloned(),
            }
        }
        "template" => Kind::Template {
            value_template: str_opt("value_template")
                .ok_or("required key not provided @ data['value_template']")?,
            for_: m.get("for").cloned(),
        },
        "homeassistant" => {
            let event = match str_opt("event").as_deref() {
                Some("start") => HaEvent::Start,
                Some("shutdown") => HaEvent::Shutdown,
                Some(other) => {
                    return Err(format!(
                        "homeassistant trigger: unsupported event '{other}'"
                    ));
                }
                None => return Err("required key not provided @ data['event']".into()),
            };
            Kind::HomeAssistant { event }
        }
        "time_pattern" => {
            let h = m.get("hours");
            let mut min = m.get("minutes").cloned();
            let mut sec = m.get("seconds").cloned();
            if h.is_none() && min.is_none() && sec.is_none() {
                return Err(
                    "time_pattern trigger: must contain at least one of hours, minutes, seconds"
                        .into(),
                );
            }
            if min.is_none() && h.is_some() {
                min = Some(json!(0));
            }
            if sec.is_none() && min.is_some() {
                sec = Some(json!(0));
            }
            Kind::TimePattern {
                hours: parse_time_expr(h, 0, 23)?,
                minutes: parse_time_expr(min.as_ref(), 0, 59)?,
                seconds: parse_time_expr(sec.as_ref(), 0, 59)?,
            }
        }
        "event" => {
            let event_types: Vec<String> =
                list(m.get("event_type")).iter().map(id_string).collect();
            if event_types.is_empty() {
                return Err("required key not provided @ data['event_type']".into());
            }
            let event_data = match m.get("event_data") {
                None | Some(Value::Null) => None,
                Some(Value::Object(o)) => Some(o.clone()),
                Some(_) => return Err("event_data: expected a dictionary".into()),
            };
            let context = match m.get("context") {
                None | Some(Value::Null) => None,
                Some(Value::Object(o)) => Some(o.clone()),
                Some(_) => return Err("context: expected a dictionary".into()),
            };
            Kind::Event {
                event_types,
                event_data,
                context,
            }
        }
        // shortcut: the other platforms (sun…)
        // come with the configs that use them; an automation using one is unavailable.
        other => return Err(format!("trigger platform '{other}' is not supported yet")),
    };
    Ok(Trigger {
        idx,
        id: m.get("id").map_or_else(|| idx.to_string(), id_string),
        alias: str_opt("alias"),
        enabled: m.get("enabled").cloned(),
        variables: m.get("variables").and_then(Value::as_object).cloned(),
        kind,
    })
}

impl Trigger {
    /// The start of the `trigger` variable (HA's `trigger_data` plus the platform).
    fn data(&self, platform: &str) -> Map<String, Value> {
        let mut d = Map::new();
        d.insert("id".into(), self.id.clone().into());
        d.insert("idx".into(), self.idx.to_string().into());
        d.insert("alias".into(), self.alias.clone().into());
        d.insert("platform".into(), platform.into());
        d
    }
}

#[derive(Debug, Clone)]
struct TemplateState {
    prev: bool,
    listeners: template::Listeners,
}

pub struct Automation {
    entity: Arc<ScriptEntity>,
    triggers: Vec<Trigger>,
    conditions: Vec<Condition>,
    /// Its armed triggers' tasks (time timers, MQTT listeners), stopped when it's removed.
    tasks: Mutex<Vec<AbortHandle>>,
    /// (trigger index, entity) -> the timer waiting out a state/numeric_state/template trigger's `for:`.
    pending: Mutex<HashMap<(usize, String), AbortHandle>>,
    /// (trigger index, entity) -> whether the numeric_state trigger is currently in range.
    numeric_armed: Mutex<HashMap<(usize, String), bool>>,
    /// trigger index -> last evaluated boolean and listeners for template triggers.
    template_state: Mutex<HashMap<usize, TemplateState>>,
}

impl Automation {
    /// From one `automation:` entry, the `n`th; `taken`: entity ids already in use.
    fn new(
        n: usize,
        raw: &Value,
        config_dir: &std::path::Path,
        registry: &crate::registry::Registry,
        taken: &dyn Fn(&str) -> bool,
    ) -> Self {
        let (expanded, blueprint_inputs, bp_err) =
            match crate::blueprint::expand_automation(config_dir, raw) {
                Ok((v, bp)) => (v, bp, None),
                Err(e) => (raw.clone(), None, Some(e)),
            };
        let m = expanded.as_object().cloned().unwrap_or_default();
        let id = m.get("id").map(id_string);
        let name = m
            .get("alias")
            .and_then(Value::as_str)
            .map_or_else(|| format!("automation {n}"), String::from);
        let entity_id = id
            .as_deref()
            .and_then(|id| registry.entity("automation", "automation", Some(id)))
            .and_then(|e| e["entity_id"].as_str())
            .map(String::from)
            .unwrap_or_else(|| {
                let base = format!("automation.{}", slugify(&name));
                let mut eid = base.clone();
                let mut k = 2;
                while taken(&eid) || registry.entity_id_taken(&eid) {
                    eid = format!("{base}_{k}");
                    k += 1;
                }
                eid
            });
        let triggers = list(plural(&m, "triggers"))
            .iter()
            .enumerate()
            .map(|(i, t)| parse_trigger(i, t))
            .collect::<Result<Vec<_>, _>>();
        let conditions = list(plural(&m, "conditions"))
            .iter()
            .map(Condition::parse)
            .collect::<Result<Vec<_>, _>>();
        let check = match (bp_err.as_ref(), &triggers, &conditions) {
            (Some(e), _, _) | (None, Err(e), _) | (None, _, Err(e)) => Err(e.clone()),
            _ => Ok(()),
        };
        let key = entity_id.split_once('.').map_or("", |(_, o)| o).to_string();
        // shortcut: an automation without an `id` stores its traces under its object id;
        // HA has no trace for it (the frontend can't open one without an id).
        let item_id = id.clone().unwrap_or_else(|| key.clone());
        let entity = ScriptEntity::new(
            "automation",
            (&key, &item_id, id.as_deref()),
            entity_id,
            &name,
            &expanded,
            plural(&m, "actions").unwrap_or(&Value::Null),
            check,
        )
        .with_blueprint_inputs(blueprint_inputs);
        Self {
            entity: Arc::new(entity),
            triggers: triggers.unwrap_or_default(),
            conditions: conditions.unwrap_or_default(),
            tasks: Mutex::default(),
            pending: Mutex::default(),
            numeric_armed: Mutex::default(),
            template_state: Mutex::default(),
        }
    }

    /// The `id` it's edited and reloaded by.
    fn id(&self) -> Option<String> {
        self.entity.raw.get("id").map(id_string)
    }

    /// Stop its `for:` timers (turned off).
    fn cancel_pending(&self) {
        for (_, h) in self.pending.lock().unwrap().drain() {
            h.abort();
        }
    }

    /// Detach its triggers and stop its runs (removed or replaced).
    async fn unload(&self) {
        self.cancel_pending();
        for h in self.tasks.lock().unwrap().drain(..) {
            h.abort();
        }
        self.entity.stop().await;
    }
}

/// All automations; replaced one by one when the config is reloaded.
#[derive(Default)]
pub struct Automations {
    items: RwLock<Vec<Arc<Automation>>>,
}

impl Automations {
    /// From `automation:` in the config; writes each entity's initial state.
    pub fn load(
        config: &[Value],
        config_dir: &std::path::Path,
        registry: &crate::registry::Registry,
        states: &States,
        restore: &crate::restore::Restore,
    ) -> Self {
        let mut items: Vec<Arc<Automation>> = vec![];
        for (n, raw) in config.iter().enumerate() {
            let taken = |eid: &str| items.iter().any(|a| a.entity.entity_id == eid);
            let a = Automation::new(n, raw, config_dir, registry, &taken);
            a.entity.restore_from(restore, registry);
            a.entity.write(states);
            items.push(Arc::new(a));
        }
        Self {
            items: RwLock::new(items),
        }
    }

    pub fn entity_ids(&self) -> Vec<String> {
        self.all()
            .iter()
            .map(|a| a.entity.entity_id.clone())
            .collect()
    }

    fn all(&self) -> Vec<Arc<Automation>> {
        self.items.read().unwrap().clone()
    }

    /// `automation.reload` (HA's `async_process_config`, or `async_process_single_config` with
    /// an `id`, as the editor's save calls it): re-read the config; an automation whose config
    /// didn't change keeps running, a changed one is replaced (keeping on/off and
    /// `last_triggered`), a new one is added, a removed one stops and is shown unavailable.
    pub async fn reload(&self, app: &Arc<App>, only_id: Option<&str>) -> Result<(), String> {
        let config = crate::config::load(&app.config_dir)?;
        let old = self.all();
        let wanted = |id: Option<String>| only_id.is_none() || id.as_deref() == only_id;
        let mut next: Vec<Arc<Automation>> =
            old.iter().filter(|a| !wanted(a.id())).cloned().collect();
        let mut added = vec![];
        for (n, raw) in config.automations.iter().enumerate() {
            if !wanted(raw.get("id").map(id_string)) {
                continue;
            }
            let probe = Automation::new(n, raw, &app.config_dir, &app.registry, &|_| false);
            let same = |a: &&Arc<Automation>| {
                a.entity.raw == probe.entity.raw
                    && a.entity.name == probe.entity.name
                    && a.entity.blueprint_inputs == probe.entity.blueprint_inputs
            };
            if let Some(kept) = old
                .iter()
                .filter(|a| !next.iter().any(|b| Arc::ptr_eq(a, b)))
                .find(same)
            {
                next.push(kept.clone());
                continue;
            }
            // A replacement keeps the entity id of the one it replaces (HA's registry entry).
            let prev = probe.id().and_then(|id| {
                old.iter()
                    .find(|a| {
                        a.id().as_deref() == Some(&id) && !next.iter().any(|b| Arc::ptr_eq(a, b))
                    })
                    .cloned()
            });
            let a = match &prev {
                Some(p) => {
                    let reuse = p.entity.entity_id.clone();
                    let mut a = Automation::new(n, raw, &app.config_dir, &app.registry, &|_| false);
                    if a.entity.entity_id != reuse {
                        a = Automation::new(n, raw, &app.config_dir, &app.registry, &|eid| {
                            eid != reuse
                        });
                    }
                    a
                }
                None => {
                    let taken = |eid: &str| {
                        app.states.get(eid).is_some()
                            || next.iter().any(|b| b.entity.entity_id == eid)
                    };
                    Automation::new(n, raw, &app.config_dir, &app.registry, &taken)
                }
            };
            let a = Arc::new(a);
            next.push(a.clone());
            added.push((a, prev));
        }
        for a in &old {
            if !next.iter().any(|b| Arc::ptr_eq(a, b)) {
                a.unload().await;
                a.entity.forget(app);
                a.entity.write_removed(&app.states);
            }
        }
        *self.items.write().unwrap() = next;
        for (a, prev) in added {
            match &prev {
                Some(p) => a.entity.restore(&p.entity),
                None => a.entity.restore_from(&app.restore, &app.registry),
            }
            a.entity.write(&app.states);
            arm(app, &a);
        }
        Ok(())
    }

    /// The config editor deleted an automation: its entity goes away (HA removes its registry
    /// entry).
    pub async fn remove(&self, app: &App, id: &str) {
        let gone: Vec<_> = {
            let mut items = self.items.write().unwrap();
            let (gone, keep) = items.drain(..).partition(|a| a.id().as_deref() == Some(id));
            *items = keep;
            gone
        };
        for a in gone {
            a.unload().await;
            a.entity.forget(app);
            app.states.remove(&a.entity.entity_id);
        }
    }
}

/// Arm every automation's triggers (HA does it once started), and listen for the state changes
/// and events that triggers wait for. Call after MQTT has started.
pub fn start(app: &Arc<App>) {
    for a in app.automations.all() {
        arm(app, &a);
    }
    let (mut rx, _) = app.states.subscribe();
    let mut ev_rx = app.states.subscribe_events();
    let app2 = app.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                r = rx.recv() => match r {
                    Ok(change) => on_state(&app2, &change),
                    Err(RecvError::Lagged(k)) => {
                        eprintln!("automation: {k} state changes dropped (too slow)")
                    }
                    Err(RecvError::Closed) => return,
                },
                r = ev_rx.recv() => match r {
                    Ok(ev) => on_event(&app2, &ev),
                    Err(RecvError::Lagged(k)) => {
                        eprintln!("automation: {k} events dropped (too slow)")
                    }
                    Err(RecvError::Closed) => return,
                },
            }
        }
    });
    // Fire `homeassistant: start` triggers after all triggers are armed.
    for a in app.automations.all() {
        if !a.entity.available() || !a.entity.enabled() {
            continue;
        }
        for t in &a.triggers {
            if matches!(
                t.kind,
                Kind::HomeAssistant {
                    event: HaEvent::Start
                }
            ) && armed(app, &a, t)
            {
                fire(app, &a, t, ha_event_data(t, HaEvent::Start), None);
            }
        }
    }
}

/// Fire `homeassistant: shutdown` triggers and wait for their runs to finish.
pub async fn stop(app: &Arc<App>) {
    let mut handles = vec![];
    for a in app.automations.all() {
        if !a.entity.available() || !a.entity.enabled() {
            continue;
        }
        for t in &a.triggers {
            if matches!(
                t.kind,
                Kind::HomeAssistant {
                    event: HaEvent::Shutdown
                }
            ) && armed(app, &a, t)
                && let Some(run_vars) =
                    prepare_fire(app, &a, t, ha_event_data(t, HaEvent::Shutdown))
            {
                let (app, a) = (app.clone(), a.clone());
                handles.push(tokio::spawn(async move {
                    trigger(&app, &a, run_vars, None, false).await
                }));
            }
        }
    }
    for h in handles {
        let _ = h.await;
    }
}

fn ha_event_data(t: &Trigger, event: HaEvent) -> Map<String, Value> {
    let mut data = t.data("homeassistant");
    let (ev, desc) = match event {
        HaEvent::Start => ("start", "Home Assistant starting"),
        HaEvent::Shutdown => ("shutdown", "Home Assistant stopping"),
    };
    data.insert("event".into(), ev.into());
    data.insert("description".into(), desc.into());
    data
}

/// Attach one automation's MQTT, time, time_pattern, numeric_state and template triggers.
fn arm(app: &Arc<App>, a: &Arc<Automation>) {
    if !a.entity.available() {
        return;
    }
    let mut tasks = vec![];
    for (ti, t) in a.triggers.iter().enumerate() {
        if !armed(app, a, t) {
            continue;
        }
        match &t.kind {
            Kind::State { .. } | Kind::HomeAssistant { .. } | Kind::Event { .. } => {}
            Kind::NumericState {
                entity_ids,
                above,
                below,
                attribute,
                value_template,
                ..
            } => {
                let mut map = a.numeric_armed.lock().unwrap();
                for eid in entity_ids {
                    let in_range = eval_numeric_state(
                        app,
                        a,
                        eid,
                        above.as_ref(),
                        below.as_ref(),
                        attribute.as_deref(),
                        value_template.as_deref(),
                    )
                    .is_ok_and(|(ok, _, _)| ok);
                    map.insert((ti, eid.clone()), in_range);
                }
            }
            Kind::Template { value_template, .. } => {
                let st = eval_template_trigger(app, a, value_template);
                a.template_state.lock().unwrap().insert(ti, st);
            }
            Kind::Mqtt { topic, qos, .. } => {
                let Some(mqtt) = app.mqtt.get() else {
                    a.entity.log("ERROR", "MQTT trigger: MQTT is not set up");
                    continue;
                };
                // shortcut: the topic stays subscribed after the automation is removed; its
                // messages are ignored. Unsubscribe when no other trigger uses it.
                let mut rx = mqtt.subscribe_for(topic, *qos);
                let (app, a) = (app.clone(), a.clone());
                tasks.push(tokio::spawn(async move {
                    loop {
                        match rx.recv().await {
                            Ok(msg) => on_mqtt(&app, &a, ti, &msg),
                            Err(RecvError::Lagged(k)) => {
                                eprintln!("automation: {k} MQTT messages dropped (too slow)")
                            }
                            Err(RecvError::Closed) => return,
                        }
                    }
                }));
            }
            Kind::Time { at, .. } => {
                for &at in at {
                    tasks.push(tokio::spawn(time_trigger(app.clone(), a.clone(), ti, at)));
                }
            }
            Kind::TimePattern { .. } => {
                tasks.push(tokio::spawn(time_pattern_trigger(
                    app.clone(),
                    a.clone(),
                    ti,
                )));
            }
        }
    }
    a.tasks
        .lock()
        .unwrap()
        .extend(tasks.iter().map(|t| t.abort_handle()));
}

/// A trigger's `enabled`, rendered when it attaches, as HA does.
fn armed(app: &App, a: &Automation, t: &Trigger) -> bool {
    match &t.enabled {
        None => true,
        Some(e) => match render_complex(&app.states, e, &attach_vars(app, a)) {
            Ok(Value::Bool(b)) => b,
            Ok(v) => v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")),
            Err(err) => {
                a.entity
                    .log("ERROR", &format!("Error rendering enabled template: {err}"));
                false
            }
        },
    }
}

/// The variables triggers render with when they attach: `this`.
// shortcut: the automation's `trigger_variables` aren't rendered in; they only feed templates
// in trigger options, which aren't supported beyond `for:` and `enabled:` yet.
fn attach_vars(app: &App, a: &Automation) -> Map<String, Value> {
    let mut v = Map::new();
    v.insert(
        "this".into(),
        app.states
            .get(&a.entity.entity_id)
            .map_or(Value::Null, |s| s.json()),
    );
    v
}

fn prepare_fire(
    app: &App,
    a: &Automation,
    t: &Trigger,
    data: Map<String, Value>,
) -> Option<Map<String, Value>> {
    if !a.entity.enabled() {
        return None;
    }
    let mut run_vars = Map::new();
    // HA's `_trigger_action_wrapper`: the trigger's own `variables`, rendered with `trigger`.
    if let Some(defs) = &t.variables {
        let mut scope = Map::new();
        scope.insert("trigger".into(), data.clone().into());
        for (k, v) in defs {
            match render_complex(&app.states, v, &scope) {
                Ok(r) => {
                    scope.insert(k.clone(), r.clone());
                    run_vars.insert(k.clone(), r);
                }
                Err(e) => {
                    a.entity
                        .log("ERROR", &format!("Error rendering trigger variables: {e}"));
                    return None;
                }
            }
        }
    }
    run_vars.insert("trigger".into(), data.into());
    Some(run_vars)
}

/// A trigger fired: run the automation in the background, if it's on.
fn fire(
    app: &Arc<App>,
    a: &Arc<Automation>,
    t: &Trigger,
    data: Map<String, Value>,
    ctx: Option<Context>,
) {
    let Some(run_vars) = prepare_fire(app, a, t, data) else {
        return;
    };
    let (app, a) = (app.clone(), a.clone());
    tokio::spawn(async move { trigger(&app, &a, run_vars, ctx.as_ref(), false).await });
}

struct StateMatch<'a> {
    data: Map<String, Value>,
    old: Value,
    new: Value,
    attribute: Option<String>,
    keep_from: bool,
    for_: &'a Option<Value>,
}

/// Match a `state` trigger against a state change.
fn match_state<'a>(t: &'a Trigger, change: &Change) -> Option<StateMatch<'a>> {
    let Kind::State {
        entity_ids,
        from,
        to,
        match_all,
        attribute,
        for_,
        keep_from,
    } = &t.kind
    else {
        return None;
    };
    if !entity_ids.iter().any(|e| **e == *change.entity_id) {
        return None;
    }
    let value = |s: &Option<Arc<crate::state::State>>| match (s, attribute) {
        (None, _) => Value::Null,
        (Some(s), None) => Value::String(s.state.clone()),
        (Some(s), Some(a)) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
    };
    let (old, new) = (value(&change.old), value(&change.new));
    if attribute.is_some() && old == new {
        return None;
    }
    if !from.test(&old) || !to.test(&new) || (!match_all && old == new) {
        return None;
    }
    let json = |s: &Option<Arc<crate::state::State>>| s.as_ref().map(|s| s.json());
    let mut data = t.data("state");
    data.insert("entity_id".into(), (*change.entity_id).into());
    data.insert("from_state".into(), json(&change.old).into());
    data.insert("to_state".into(), json(&change.new).into());
    data.insert("for".into(), Value::Null);
    data.insert("attribute".into(), attribute.clone().into());
    data.insert(
        "description".into(),
        format!("state of {}", change.entity_id).into(),
    );
    Some(StateMatch {
        data,
        old,
        new,
        attribute: attribute.clone(),
        keep_from: *keep_from,
        for_,
    })
}

/// HA's `state_automation_listener`, for every automation's state, numeric_state, and template triggers.
fn on_state(app: &Arc<App>, change: &Change) {
    let lifecycle = change.old.is_none() || change.new.is_none();
    for a in app.automations.all() {
        if !a.entity.available() || !a.entity.enabled() {
            continue;
        }
        for t in &a.triggers {
            match &t.kind {
                Kind::State { .. } => on_state_trigger(app, &a, t, change),
                Kind::NumericState { .. } => on_numeric_state_trigger(app, &a, t, change),
                Kind::Template { .. } => on_template_trigger(app, &a, t, change, lifecycle),
                _ => {}
            }
        }
    }
}

fn on_state_trigger(app: &Arc<App>, a: &Arc<Automation>, t: &Trigger, change: &Change) {
    let Kind::State {
        entity_ids,
        from,
        to,
        match_all,
        attribute,
        for_,
        keep_from,
    } = &t.kind
    else {
        return;
    };
    if !entity_ids.iter().any(|e| **e == *change.entity_id) {
        return;
    }
    let value = |s: &Option<Arc<crate::state::State>>| match (s, attribute) {
        (None, _) => Value::Null,
        (Some(s), None) => Value::String(s.state.clone()),
        (Some(s), Some(a)) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
    };
    let (old, new) = (value(&change.old), value(&change.new));
    if attribute.is_some() && old == new {
        return;
    }
    if !from.test(&old) || !to.test(&new) || (!match_all && old == new) {
        return;
    }
    let json = |s: &Option<Arc<crate::state::State>>| s.as_ref().map(|s| s.json());
    let mut data = t.data("state");
    data.insert("entity_id".into(), (*change.entity_id).into());
    data.insert("from_state".into(), json(&change.old).into());
    data.insert("to_state".into(), json(&change.new).into());
    data.insert("for".into(), Value::Null);
    data.insert("attribute".into(), attribute.clone().into());
    data.insert(
        "description".into(),
        format!("state of {}", change.entity_id).into(),
    );
    let Some(for_) = for_ else {
        fire(app, a, t, data, Some(change.context.clone()));
        return;
    };
    // `for:`: fire once the entity has kept the matched state that long.
    let mut scope = attach_vars(app, a);
    scope.insert(
        "trigger".into(),
        json!({"platform": "state", "entity_id": &*change.entity_id,
            "from_state": data["from_state"], "to_state": data["to_state"]}),
    );
    let secs = match render_complex(&app.states, for_, &scope).and_then(|v| time_period(&v)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Error rendering '{}' for template: {e}", a.entity.name);
            return;
        }
    };
    // HA's JSON encoder writes a timedelta this way.
    // shortcut: templates see this dict too, where HA's see a timedelta
    // (`trigger.for.total_seconds()` fails); add a timedelta value to the template engine.
    data.insert(
        "for".into(),
        json!({"__type": "<class 'datetime.timedelta'>", "total_seconds": secs}),
    );
    let keep_from = *keep_from;
    let same = {
        let attribute = attribute.clone();
        move |s: &crate::state::State| {
            let cur = match &attribute {
                None => Value::String(s.state.clone()),
                Some(a) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
            };
            if keep_from { cur != old } else { cur == new }
        }
    };
    // Subscribed now, so no change between this one and the timer's start is missed.
    let (mut rx, _) = app.states.subscribe();
    let (app2, a2, entity, ctx) = (
        app.clone(),
        a.clone(),
        change.entity_id.clone(),
        change.context.clone(),
    );
    let ti = t.idx;
    let handle = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                r = rx.recv() => match r {
                    Ok(c) if c.entity_id == entity => {
                        if !c.new.as_deref().is_some_and(&same) {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                    _ => {}
                },
            }
        }
        fire(&app2, &a2, &a2.triggers[ti], data, Some(ctx));
    });
    let key = (t.idx, change.entity_id.to_string());
    if let Some(old) = a.pending.lock().unwrap().insert(key, handle.abort_handle()) {
        old.abort();
    }
}

fn bound_entity(b: Option<&Value>) -> Option<&str> {
    b.and_then(Value::as_str).filter(|s| s.contains('.'))
}

fn resolve_numeric_bound(app: &App, b: &Value) -> Result<Option<f64>, String> {
    match b {
        Value::Number(n) => Ok(n.as_f64()),
        Value::String(id) => {
            let Some(s) = app.states.get(id) else {
                return Ok(None);
            };
            if s.state == "unavailable" || s.state == "unknown" {
                return Ok(None);
            }
            s.state
                .trim()
                .parse::<f64>()
                .map(Some)
                .map_err(|_| format!("bound entity {id} state '{}' is not numeric", s.state))
        }
        _ => Ok(None),
    }
}

/// Returns `Ok((in_range, above_val, below_val))` for `entity_id` against `numeric_state` bounds.
fn eval_numeric_state(
    app: &App,
    a: &Automation,
    entity_id: &str,
    above: Option<&Value>,
    below: Option<&Value>,
    attribute: Option<&str>,
    value_template: Option<&str>,
) -> Result<(bool, Option<f64>, Option<f64>), String> {
    let Some(s) = app.states.get(entity_id) else {
        return Ok((false, None, None));
    };
    if let Some(attr) = attribute
        && !s.attributes.contains_key(attr)
    {
        return Ok((false, None, None));
    }
    let value: Value = match value_template {
        Some(tpl) => {
            let mut vars = attach_vars(app, a);
            vars.insert("state".into(), s.json());
            let (text, _) = template::render(&app.states, tpl, &Value::Object(vars), false)?;
            template::result(tpl, &text)
        }
        None => match attribute {
            Some(attr) => s.attributes.get(attr).cloned().unwrap_or(Value::Null),
            None => Value::String(s.state.clone()),
        },
    };
    if value.is_null() || value == "unavailable" || value == "unknown" {
        return Ok((false, None, None));
    }
    let fvalue = match &value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    };
    let Some(fvalue) = fvalue else {
        return Ok((false, None, None));
    };
    let above_val = above
        .map(|b| resolve_numeric_bound(app, b))
        .transpose()?
        .flatten();
    let below_val = below
        .map(|b| resolve_numeric_bound(app, b))
        .transpose()?
        .flatten();
    if above.is_some() && above_val.is_none() {
        return Ok((false, above_val, below_val));
    }
    if below.is_some() && below_val.is_none() {
        return Ok((false, above_val, below_val));
    }
    let ok = above_val.is_none_or(|a| fvalue > a) && below_val.is_none_or(|b| fvalue < b);
    Ok((ok, above_val, below_val))
}

fn on_numeric_state_trigger(app: &Arc<App>, a: &Arc<Automation>, t: &Trigger, change: &Change) {
    let Kind::NumericState {
        entity_ids,
        above,
        below,
        attribute,
        value_template,
        for_,
    } = &t.kind
    else {
        return;
    };
    let changed_bound = bound_entity(above.as_ref()) == Some(&*change.entity_id)
        || bound_entity(below.as_ref()) == Some(&*change.entity_id);
    if !changed_bound && !entity_ids.iter().any(|e| **e == *change.entity_id) {
        return;
    }
    let targets: Vec<String> = if changed_bound {
        entity_ids.clone()
    } else {
        vec![change.entity_id.to_string()]
    };
    for eid in targets {
        let (in_range, above_val, below_val) = match eval_numeric_state(
            app,
            a,
            &eid,
            above.as_ref(),
            below.as_ref(),
            attribute.as_deref(),
            value_template.as_deref(),
        ) {
            Ok(v) => v,
            Err(e) => {
                a.entity
                    .log("ERROR", &format!("numeric_state trigger: {e}"));
                continue;
            }
        };
        let was_in_range = {
            let mut map = a.numeric_armed.lock().unwrap();
            let prev = map.get(&(t.idx, eid.clone())).copied().unwrap_or(false);
            map.insert((t.idx, eid.clone()), in_range);
            prev
        };
        if !in_range {
            if let Some(old) = a.pending.lock().unwrap().remove(&(t.idx, eid.clone())) {
                old.abort();
            }
            continue;
        }
        if was_in_range {
            continue;
        }
        let json = |s: &Option<Arc<crate::state::State>>| s.as_ref().map(|s| s.json());
        let mut data = t.data("numeric_state");
        data.insert("entity_id".into(), eid.clone().into());
        data.insert("below".into(), below_val.map_or(Value::Null, |v| json!(v)));
        data.insert("above".into(), above_val.map_or(Value::Null, |v| json!(v)));
        if eid == *change.entity_id {
            data.insert("from_state".into(), json(&change.old).into());
            data.insert("to_state".into(), json(&change.new).into());
        } else {
            let cur = app.states.get(&eid).map(|s| s.json());
            data.insert("from_state".into(), cur.clone().into());
            data.insert("to_state".into(), cur.into());
        }
        data.insert("for".into(), Value::Null);
        data.insert(
            "description".into(),
            format!("numeric state of {eid}").into(),
        );
        let Some(for_) = for_ else {
            fire(app, a, t, data, Some(change.context.clone()));
            continue;
        };
        let mut scope = attach_vars(app, a);
        scope.insert("trigger".into(), data.clone().into());
        let secs = match render_complex(&app.states, for_, &scope).and_then(|v| time_period(&v)) {
            Ok(s) => s,
            Err(e) => {
                a.entity
                    .log("ERROR", &format!("Error rendering numeric_state for: {e}"));
                continue;
            }
        };
        data.insert(
            "for".into(),
            json!({"__type": "<class 'datetime.timedelta'>", "total_seconds": secs}),
        );
        let (mut rx, _) = app.states.subscribe();
        let (app2, a2, ctx, eid2) = (app.clone(), a.clone(), change.context.clone(), eid.clone());
        let ti = t.idx;
        let above_c = above.clone();
        let below_c = below.clone();
        let attr_c = attribute.clone();
        let vt_c = value_template.clone();
        let handle = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
            loop {
                tokio::select! {
                    _ = tokio::time::sleep_until(deadline) => break,
                    r = rx.recv() => match r {
                        Ok(c) if *c.entity_id == eid2
                            || bound_entity(above_c.as_ref()) == Some(&*c.entity_id)
                            || bound_entity(below_c.as_ref()) == Some(&*c.entity_id) => {
                            let ok = eval_numeric_state(
                                &app2, &a2, &eid2, above_c.as_ref(), below_c.as_ref(),
                                attr_c.as_deref(), vt_c.as_deref()
                            ).is_ok_and(|(ok, _, _)| ok);
                            if !ok {
                                return;
                            }
                        }
                        Err(RecvError::Closed) => return,
                        _ => {}
                    }
                }
            }
            fire(&app2, &a2, &a2.triggers[ti], data, Some(ctx));
        });
        if let Some(old) = a
            .pending
            .lock()
            .unwrap()
            .insert((t.idx, eid), handle.abort_handle())
        {
            old.abort();
        }
    }
}

fn eval_template_trigger(app: &App, a: &Automation, tpl: &str) -> TemplateState {
    let vars = Value::Object(attach_vars(app, a));
    match template::render(&app.states, tpl, &vars, false) {
        Ok((text, listeners)) => TemplateState {
            prev: text.eq_ignore_ascii_case("true"),
            listeners,
        },
        Err(e) => {
            a.entity.log("ERROR", &format!("template trigger: {e}"));
            TemplateState {
                prev: false,
                listeners: template::Listeners::default(),
            }
        }
    }
}

fn on_template_trigger(
    app: &Arc<App>,
    a: &Arc<Automation>,
    t: &Trigger,
    change: &Change,
    lifecycle: bool,
) {
    let Kind::Template {
        value_template,
        for_,
    } = &t.kind
    else {
        return;
    };
    let should_eval = {
        let map = a.template_state.lock().unwrap();
        map.get(&t.idx)
            .is_some_and(|st| st.listeners.matches(&change.entity_id, lifecycle))
    };
    if !should_eval {
        return;
    }
    let next = eval_template_trigger(app, a, value_template);
    let next_prev = next.prev;
    let prev = {
        let mut map = a.template_state.lock().unwrap();
        let prev = map.get(&t.idx).is_some_and(|st| st.prev);
        map.insert(t.idx, next);
        prev
    };
    if !next_prev {
        if let Some(old) = a.pending.lock().unwrap().remove(&(t.idx, String::new())) {
            old.abort();
        }
        return;
    }
    if prev {
        return;
    }
    let json = |s: &Option<Arc<crate::state::State>>| s.as_ref().map(|s| s.json());
    let mut data = t.data("template");
    data.insert("entity_id".into(), (*change.entity_id).into());
    data.insert("from_state".into(), json(&change.old).into());
    data.insert("to_state".into(), json(&change.new).into());
    data.insert("for".into(), Value::Null);
    data.insert(
        "description".into(),
        format!("{} via template", change.entity_id).into(),
    );
    let Some(for_) = for_ else {
        fire(app, a, t, data, Some(change.context.clone()));
        return;
    };
    let mut scope = attach_vars(app, a);
    scope.insert("trigger".into(), data.clone().into());
    let secs = match render_complex(&app.states, for_, &scope).and_then(|v| time_period(&v)) {
        Ok(s) => s,
        Err(e) => {
            a.entity.log(
                "ERROR",
                &format!("Error rendering template trigger for: {e}"),
            );
            return;
        }
    };
    data.insert(
        "for".into(),
        json!({"__type": "<class 'datetime.timedelta'>", "total_seconds": secs}),
    );
    let (mut rx, _) = app.states.subscribe();
    let (app2, a2, ctx) = (app.clone(), a.clone(), change.context.clone());
    let ti = t.idx;
    let tpl = value_template.clone();
    let handle = tokio::spawn(async move {
        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
        loop {
            tokio::select! {
                _ = tokio::time::sleep_until(deadline) => break,
                r = rx.recv() => match r {
                    Ok(c) => {
                        let lc = c.old.is_none() || c.new.is_none();
                        let matches = a2.template_state.lock().unwrap().get(&ti)
                            .is_some_and(|st| st.listeners.matches(&c.entity_id, lc));
                        if matches {
                            let st = eval_template_trigger(&app2, &a2, &tpl);
                            let ok = st.prev;
                            a2.template_state.lock().unwrap().insert(ti, st);
                            if !ok {
                                return;
                            }
                        }
                    }
                    Err(RecvError::Closed) => return,
                    Err(RecvError::Lagged(_)) => {}
                }
            }
        }
        fire(&app2, &a2, &a2.triggers[ti], data, Some(ctx));
    });
    if let Some(old) = a
        .pending
        .lock()
        .unwrap()
        .insert((t.idx, String::new()), handle.abort_handle())
    {
        old.abort();
    }
}

/// Match an `event` trigger against a fired event.
fn match_event(
    t: &Trigger,
    ev: &Event,
    states: &Arc<States>,
    scope: &Map<String, Value>,
) -> Option<Map<String, Value>> {
    let Kind::Event {
        event_types,
        event_data,
        context,
    } = &t.kind
    else {
        return None;
    };
    let type_matched = event_types.iter().any(|et| {
        if template::is_static(et) {
            et == &ev.event_type
        } else {
            render_complex(states, &Value::String(et.clone()), scope)
                .ok()
                .and_then(|v| v.as_str().map(String::from))
                .is_some_and(|s| s == ev.event_type)
        }
    });
    if !type_matched {
        return None;
    }
    if let Some(expected) = event_data {
        for (k, v) in expected {
            let want = render_complex(states, v, scope).ok()?;
            if ev.data.get(k) != Some(&want) {
                return None;
            }
        }
    }
    if let Some(expected_ctx) = context {
        let ev_ctx = ev.context.json();
        for (k, v) in expected_ctx {
            let want = render_complex(states, v, scope).ok()?;
            if ev_ctx.get(k) != Some(&want) {
                return None;
            }
        }
    }
    let mut data = t.data("event");
    data.insert("event".into(), ev.json());
    data.insert(
        "description".into(),
        format!("event '{}'", ev.event_type).into(),
    );
    Some(data)
}

/// HA's `event_automation_listener`, for every automation's event triggers.
fn on_event(app: &Arc<App>, ev: &Event) {
    for a in app.automations.all() {
        if !a.entity.available() || !a.entity.enabled() {
            continue;
        }
        let scope = attach_vars(app, &a);
        for t in &a.triggers {
            if let Some(data) = match_event(t, ev, &app.states, &scope) {
                fire(app, &a, t, data, Some(ev.context.clone()));
            }
        }
    }
}

/// Match an `mqtt` trigger against a received message.
fn match_mqtt(
    t: &Trigger,
    msg: &crate::mqtt::Message,
    states: &Arc<States>,
    scope: &Map<String, Value>,
    on_err: &dyn Fn(&str),
) -> Option<Map<String, Value>> {
    let Kind::Mqtt {
        topic,
        payload,
        value_template,
        ..
    } = &t.kind
    else {
        return None;
    };
    if !topic_matches(topic, &msg.topic) {
        return None;
    }
    let json_payload = serde_json::from_str::<Value>(&msg.payload).ok();
    if let Some(wanted) = payload {
        // The wanted payload is a template rendered with the attach variables.
        let wanted = match template::is_static(wanted) {
            true => wanted.clone(),
            false => match template::render(states, wanted, &Value::Object(scope.clone()), false) {
                Ok((s, _)) => s,
                Err(e) => {
                    on_err(&format!("payload template: {e}"));
                    return None;
                }
            },
        };
        let got = match value_template {
            None => msg.payload.clone(),
            Some(vt) => {
                let mut vars = json!({"value": msg.payload});
                if let Some(j) = &json_payload {
                    vars["value_json"] = j.clone();
                }
                match template::render(states, vt, &vars, false) {
                    Ok((s, _)) => s,
                    Err(e) => {
                        on_err(&format!("value_template: {e}"));
                        return None;
                    }
                }
            }
        };
        if got.is_empty() || got != wanted {
            return None;
        }
    }
    let mut data = t.data("mqtt");
    data.insert("topic".into(), msg.topic.clone().into());
    data.insert("payload".into(), msg.payload.clone().into());
    data.insert("qos".into(), msg.qos.into());
    data.insert(
        "description".into(),
        format!("mqtt topic {}", msg.topic).into(),
    );
    if let Some(j) = json_payload {
        data.insert("payload_json".into(), j);
    }
    Some(data)
}

/// HA's `mqtt_automation_listener`.
fn on_mqtt(app: &Arc<App>, a: &Arc<Automation>, ti: usize, msg: &crate::mqtt::Message) {
    let t = &a.triggers[ti];
    if let Some(data) = match_mqtt(t, msg, &app.states, &attach_vars(app, a), &|e| {
        a.entity.log("ERROR", e)
    }) {
        fire(app, a, t, data, None);
    }
}

/// Parsed triggers for `wait_for_trigger`.
#[derive(Debug)]
pub(crate) struct ParsedTriggers(Arc<Vec<Trigger>>);

pub(crate) fn parse_triggers(v: &Value) -> Result<ParsedTriggers, String> {
    let items = list(Some(v));
    if items.is_empty() {
        return Err("wait_for_trigger: expected at least one trigger".into());
    }
    let triggers = items
        .iter()
        .enumerate()
        .map(|(i, t)| parse_trigger(i, t))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ParsedTriggers(Arc::new(triggers)))
}

/// Wait until any trigger in `triggers` fires, `stop` is signalled, or `timeout` elapses.
pub(crate) async fn wait_for_triggers(
    app: &Arc<App>,
    triggers: &ParsedTriggers,
    vars: &Map<String, Value>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
    timeout: Option<Duration>,
) -> Result<Option<(Map<String, Value>, Context)>, Halt> {
    let (hit_tx, mut hit_rx) =
        tokio::sync::mpsc::unbounded_channel::<(Map<String, Value>, Context)>();
    let mut set = tokio::task::JoinSet::new();
    let has_state = triggers
        .0
        .iter()
        .any(|t| matches!(t.kind, Kind::State { .. }));
    let has_event = triggers
        .0
        .iter()
        .any(|t| matches!(t.kind, Kind::Event { .. }));
    if has_state {
        let (mut rx, _) = app.states.subscribe();
        let (app2, trigs, vars2, tx) = (
            app.clone(),
            triggers.0.clone(),
            vars.clone(),
            hit_tx.clone(),
        );
        set.spawn(async move {
            let mut pending: HashMap<(usize, String), AbortHandle> = HashMap::new();
            loop {
                let change = match rx.recv().await {
                    Ok(c) => c,
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                };
                for t in trigs.iter() {
                    let Some(StateMatch {
                        mut data,
                        old,
                        new,
                        attribute,
                        keep_from,
                        for_,
                    }) = match_state(t, &change)
                    else {
                        continue;
                    };
                    let Some(for_) = for_ else {
                        let _ = tx.send((data, change.context.clone()));
                        return;
                    };
                    let mut scope = vars2.clone();
                    scope.insert(
                        "trigger".into(),
                        json!({"platform": "state", "entity_id": &*change.entity_id,
                            "from_state": data["from_state"], "to_state": data["to_state"]}),
                    );
                    let Ok(secs) =
                        render_complex(&app2.states, for_, &scope).and_then(|v| time_period(&v))
                    else {
                        continue;
                    };
                    data.insert(
                        "for".into(),
                        json!({"__type": "<class 'datetime.timedelta'>", "total_seconds": secs}),
                    );
                    let same = {
                        let attribute = attribute.clone();
                        move |s: &crate::state::State| {
                            let cur = match &attribute {
                                None => Value::String(s.state.clone()),
                                Some(a) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
                            };
                            if keep_from { cur != old } else { cur == new }
                        }
                    };
                    let (mut rx2, _) = app2.states.subscribe();
                    let (entity, ctx, tx2) =
                        (change.entity_id.clone(), change.context.clone(), tx.clone());
                    let handle = tokio::spawn(async move {
                        let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(secs);
                        loop {
                            tokio::select! {
                                _ = tokio::time::sleep_until(deadline) => break,
                                r = rx2.recv() => match r {
                                    Ok(c) if c.entity_id == entity => {
                                        if !c.new.as_deref().is_some_and(&same) {
                                            return;
                                        }
                                    }
                                    Err(RecvError::Closed) => return,
                                    _ => {}
                                },
                            }
                        }
                        let _ = tx2.send((data, ctx));
                    });
                    if let Some(old_h) =
                        pending.insert((t.idx, change.entity_id.to_string()), handle.abort_handle())
                    {
                        old_h.abort();
                    }
                }
            }
        });
    }
    if has_event {
        let mut ev_rx = app.states.subscribe_events();
        let (states, trigs, vars2, tx) = (
            app.states.clone(),
            triggers.0.clone(),
            vars.clone(),
            hit_tx.clone(),
        );
        set.spawn(async move {
            loop {
                let ev = match ev_rx.recv().await {
                    Ok(e) => e,
                    Err(RecvError::Closed) => break,
                    Err(RecvError::Lagged(_)) => continue,
                };
                for t in trigs.iter() {
                    if let Some(data) = match_event(t, &ev, &states, &vars2) {
                        let _ = tx.send((data, ev.context.clone()));
                        return;
                    }
                }
            }
        });
    }
    for (ti, t) in triggers.0.iter().enumerate() {
        match &t.kind {
            Kind::Mqtt { topic, qos, .. } => {
                let Some(mqtt) = app.mqtt.get() else {
                    continue;
                };
                let mut rx = mqtt.subscribe_for(topic, *qos);
                let (states, trigs, vars2, tx) = (
                    app.states.clone(),
                    triggers.0.clone(),
                    vars.clone(),
                    hit_tx.clone(),
                );
                set.spawn(async move {
                    loop {
                        let msg = match rx.recv().await {
                            Ok(m) => m,
                            Err(RecvError::Closed) => break,
                            Err(RecvError::Lagged(_)) => continue,
                        };
                        if let Some(data) = match_mqtt(&trigs[ti], &msg, &states, &vars2, &|_| {}) {
                            let _ = tx.send((data, Context::new(None)));
                            return;
                        }
                    }
                });
            }
            Kind::Time { at, weekdays } => {
                for &at in at {
                    let (app2, trigs, weekdays, tx) = (
                        app.clone(),
                        triggers.0.clone(),
                        weekdays.clone(),
                        hit_tx.clone(),
                    );
                    set.spawn(async move {
                        let tz = &app2.config.core.tz;
                        let mut after = Utc::now();
                        loop {
                            let next = next_time(tz, Utc::now().max(after), at);
                            let left = (next - Utc::now()).to_std().unwrap_or_default();
                            if left > Duration::from_secs(60) {
                                tokio::time::sleep(Duration::from_secs(60)).await;
                                continue;
                            }
                            tokio::time::sleep(left).await;
                            after = next;
                            let local = next + tz.offset(next);
                            if let Some(days) = &weekdays
                                && !days.iter().any(|d| {
                                    *d == WEEKDAYS[local.weekday().num_days_from_monday() as usize]
                                })
                            {
                                continue;
                            }
                            let mut data = trigs[ti].data("time");
                            data.insert(
                                "now".into(),
                                Utc::now()
                                    .with_timezone(&tz.offset(Utc::now()))
                                    .format("%Y-%m-%dT%H:%M:%S%.6f%:z")
                                    .to_string()
                                    .into(),
                            );
                            data.insert("description".into(), "time".into());
                            data.insert("entity_id".into(), Value::Null);
                            let _ = tx.send((data, Context::new(None)));
                            return;
                        }
                    });
                }
            }
            Kind::State { .. }
            | Kind::Event { .. }
            | Kind::NumericState { .. }
            | Kind::Template { .. }
            | Kind::HomeAssistant { .. }
            | Kind::TimePattern { .. } => {}
        }
    }
    let deadline = timeout.map(|d| tokio::time::Instant::now() + d);
    tokio::select! {
        hit = hit_rx.recv() => {
            drop(set);
            Ok(hit)
        }
        _ = stop.wait_for(|s| *s) => {
            drop(set);
            Err(Halt::Cancelled)
        }
        _ = async {
            match deadline {
                Some(d) => tokio::time::sleep_until(d).await,
                None => std::future::pending::<()>().await,
            }
        } => {
            drop(set);
            Ok(None)
        }
    }
}

/// The first instant after `after` whose wall-clock time in `tz` is `at`.
// shortcut: at a DST change, a repeated time fires at its first occurrence and a skipped one an
// hour later (02:30 at 03:30), unchecked against HA; check with a recording across a change.
fn next_time(tz: &crate::config::Zone, after: DateTime<Utc>, at: NaiveTime) -> DateTime<Utc> {
    let date = (after + tz.offset(after)).date_naive();
    (0..3)
        .map(|d| tz.local_to_utc((date + chrono::Days::new(d)).and_time(at)))
        .find(|t| *t > after)
        .expect("a wall-clock time recurs within 3 days")
}

/// A time trigger's timer (HA's `async_track_time_change`), firing at `at` local time daily.
async fn time_trigger(app: Arc<App>, a: Arc<Automation>, ti: usize, at: NaiveTime) {
    // In tests the wall clock follows tokio's, so a paused clock can run through a night.
    #[cfg(test)]
    let base = (Utc::now(), tokio::time::Instant::now());
    let now = || {
        #[cfg(test)]
        return base.0 + (tokio::time::Instant::now() - base.1);
        #[cfg(not(test))]
        Utc::now()
    };
    let tz = &app.config.core.tz;
    let mut after = now();
    loop {
        let next = next_time(tz, now().max(after), at);
        let left = (next - now()).to_std().unwrap_or_default();
        // Sleep at most a minute at a time and recompute from the wall clock: a board without a
        // real-time clock boots in 1970 until NTP sets the time.
        if left > Duration::from_secs(60) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        tokio::time::sleep(left).await;
        after = next;
        let t = &a.triggers[ti];
        let local = next + tz.offset(next);
        if let Kind::Time {
            weekdays: Some(days),
            ..
        } = &t.kind
            && !days
                .iter()
                .any(|d| *d == WEEKDAYS[local.weekday().num_days_from_monday() as usize])
        {
            continue;
        }
        let fired = now();
        let mut data = t.data("time");
        data.insert(
            "now".into(),
            fired
                .with_timezone(&tz.offset(fired))
                .format("%Y-%m-%dT%H:%M:%S%.6f%:z")
                .to_string()
                .into(),
        );
        data.insert("description".into(), "time".into());
        data.insert("entity_id".into(), Value::Null);
        fire(&app, &a, t, data, None);
    }
}

fn parse_time_expr(v: Option<&Value>, min: u32, max: u32) -> Result<Vec<u32>, String> {
    let Some(v) = v else {
        return Ok((min..=max).collect());
    };
    let mut out = match v {
        Value::Number(n) => {
            let x = n
                .as_u64()
                .and_then(|x| u32::try_from(x).ok())
                .filter(|x| *x >= min && *x <= max)
                .ok_or_else(|| format!("time_pattern: value {v} out of range {min}..={max}"))?;
            vec![x]
        }
        Value::String(s) => {
            let s = s.trim();
            if s == "*" {
                (min..=max).collect()
            } else if let Some(rest) = s.strip_prefix('/') {
                let step: u32 = rest
                    .parse()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| format!("time_pattern: invalid step '{s}'"))?;
                (min..=max).filter(|v| v % step == 0).collect()
            } else {
                s.split(',')
                    .map(|part| {
                        part.trim()
                            .parse::<u32>()
                            .ok()
                            .filter(|x| *x >= min && *x <= max)
                            .ok_or_else(|| {
                                format!("time_pattern: value '{part}' out of range {min}..={max}")
                            })
                    })
                    .collect::<Result<Vec<_>, _>>()?
            }
        }
        _ => return Err(format!("time_pattern: invalid expression {v}")),
    };
    out.sort_unstable();
    out.dedup();
    if out.is_empty() {
        return Err("time_pattern: expression matched no values".into());
    }
    Ok(out)
}

fn find_time_pattern_at_or_after(
    tz: &crate::config::Zone,
    at_or_after: DateTime<Utc>,
    hours: &[u32],
    mins: &[u32],
    secs: &[u32],
) -> DateTime<Utc> {
    let local = at_or_after + tz.offset(at_or_after);
    let date = local.date_naive();
    let start_hms = (local.hour(), local.minute(), local.second());
    for d in 0..3 {
        let day = date + chrono::Days::new(d);
        let min_hms = if d == 0 { start_hms } else { (0, 0, 0) };
        for &h in hours {
            if h < min_hms.0 {
                continue;
            }
            for &m in mins {
                if (h, m) < (min_hms.0, min_hms.1) {
                    continue;
                }
                for &s in secs {
                    if (h, m, s) < min_hms {
                        continue;
                    }
                    let t = NaiveTime::from_hms_opt(h, m, s).unwrap();
                    let utc = tz.local_to_utc(day.and_time(t));
                    if utc >= at_or_after {
                        return utc;
                    }
                }
            }
        }
    }
    unreachable!("a time_pattern recurs within 3 days")
}

async fn time_pattern_trigger(app: Arc<App>, a: Arc<Automation>, ti: usize) {
    #[cfg(test)]
    let base = (Utc::now(), tokio::time::Instant::now());
    let now = || {
        #[cfg(test)]
        return base.0 + (tokio::time::Instant::now() - base.1);
        #[cfg(not(test))]
        Utc::now()
    };
    let tz = &app.config.core.tz;
    let Kind::TimePattern {
        hours,
        minutes,
        seconds,
    } = &a.triggers[ti].kind
    else {
        return;
    };
    let mut after = now() + chrono::Duration::seconds(1);
    loop {
        let cur = now();
        let next = find_time_pattern_at_or_after(tz, cur.max(after), hours, minutes, seconds);
        let left = (next - cur).to_std().unwrap_or_default();
        if left > Duration::from_secs(60) {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        tokio::time::sleep(left).await;
        after = next + chrono::Duration::seconds(1);
        let t = &a.triggers[ti];
        let fired = now();
        let mut data = t.data("time_pattern");
        data.insert(
            "now".into(),
            fired
                .with_timezone(&tz.offset(fired))
                .format("%Y-%m-%dT%H:%M:%S%.6f%:z")
                .to_string()
                .into(),
        );
        data.insert("description".into(), "time pattern".into());
        data.insert("entity_id".into(), Value::Null);
        fire(&app, &a, t, data, None);
    }
}

/// Run an automation (HA's `AutomationEntity.async_trigger`): trace the trigger, check the
/// conditions, then run the actions with a context whose parent is the trigger's. Returns the
/// run's trace id.
async fn trigger(
    app: &Arc<App>,
    a: &Arc<Automation>,
    run_vars: Map<String, Value>,
    parent: Option<&Context>,
    skip_condition: bool,
) -> Option<String> {
    let e = &a.entity;
    let ctx = Context {
        parent_id: parent.map(|c| c.id.clone()),
        ..Context::new(None)
    };
    let trace = app
        .traces
        .start("automation", &e.item_id, e.raw.clone(), ctx.clone());
    trace.set_blueprint_inputs(e.blueprint_inputs.clone());
    let run_id = trace.run_id();
    let mut vars = Map::new();
    vars.insert(
        "this".into(),
        app.states
            .get(&e.entity_id)
            .map_or(Value::Null, |s| s.json()),
    );
    vars.extend(run_vars);
    if let Err(err) = e.render_variables(&app.states, &mut vars) {
        e.log("ERROR", &format!("Error rendering variables: {err}"));
        trace.finish("error", Some(err));
        return Some(run_id);
    }
    let trig = vars.get("trigger");
    trace.set_trigger(
        trig.and_then(|t| t.get("description"))
            .and_then(Value::as_str)
            .map(String::from),
    );
    let path = match trig.and_then(|t| t.get("idx")).and_then(Value::as_str) {
        Some(i) => format!("trigger/{i}"),
        None => "trigger".into(),
    };
    trace.element(&path, Some(&vars));

    if !skip_condition && !a.conditions.is_empty() {
        let ev = Eval {
            states: &app.states,
            vars: &vars,
            trace: Some(&trace),
        };
        let mut pass = true;
        let mut errors = vec![];
        for (i, c) in a.conditions.iter().enumerate() {
            match c.check(&ev, &format!("condition/{i}"), None) {
                Ok(true) => {}
                Ok(false) => {
                    pass = false;
                    break;
                }
                Err(err) => errors.push(format!(
                    "In 'condition' (item {} of {}):\n  {err}",
                    i + 1,
                    a.conditions.len()
                )),
            }
        }
        if pass && !errors.is_empty() {
            e.log(
                "WARNING",
                &format!(
                    "Error evaluating condition in '{}':\n{}",
                    e.name,
                    errors.join("\n")
                ),
            );
            pass = false;
        }
        if !pass {
            trace.finish("failed_conditions", None);
            return Some(run_id);
        }
    }
    e.set_context(&ctx);
    // A new script stack: automations may trigger themselves.
    launch(app, e.clone(), trace, Vars::Ready(vars), ctx, true, vec![]).await
}

/// The `automation` domain's services (HA's `components/automation/services.py`).
pub async fn call_service(
    app: &Arc<App>,
    service: &str,
    data: Value,
    target: Value,
    ctx: Context,
) -> Result<Outcome, (&'static str, String)> {
    match service {
        "turn_on" | "turn_off" | "toggle" | "trigger" => {}
        "reload" => {
            return app
                .automations
                .reload(app, data["id"].as_str())
                .await
                .map(|_| Outcome::default())
                .map_err(|e| ("home_assistant_error", e));
        }
        _ => {
            return Err((
                "not_found",
                format!("Service automation.{service} not found."),
            ));
        }
    }
    let autos = &app.automations;
    let ids: Vec<String> = entity_ids(&data["entity_id"])
        .into_iter()
        .chain(entity_ids(&target["entity_id"]))
        .collect();
    let all = ids.iter().any(|i| i == "all");
    let mut child = None;
    // shortcut: one entity after the other; HA calls them concurrently.
    for a in autos.all() {
        let e = &a.entity;
        // An entity service skips unavailable entities.
        if !e.available() || !(all || ids.contains(&e.entity_id)) {
            continue;
        }
        e.set_context(&ctx);
        let on = match service {
            "toggle" => !e.enabled(),
            s => s == "turn_on",
        };
        match service {
            "trigger" => {
                let mut vars = data["variables"].as_object().cloned().unwrap_or_default();
                vars.insert("trigger".into(), json!({"platform": null}));
                let skip = data["skip_condition"].as_bool().unwrap_or(true);
                let run = trigger(app, &a, vars, Some(&ctx), skip).await;
                child = run.map(|r| ("automation", e.item_id.clone(), r));
            }
            _ if on => {
                e.set_enabled(true);
                e.write(&app.states);
            }
            _ => {
                e.set_enabled(false);
                a.cancel_pending();
                if data["stop_actions"].as_bool().unwrap_or(true) {
                    e.stop().await;
                }
                e.write(&app.states);
            }
        }
    }
    Ok(Outcome { child_run: child })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::tests::{norm, recording};

    const USER: &str = "0123456789abcdef0123456789abcdef";

    /// lean-home with the scenario's automations and script (or `automations` instead), and
    /// the scenario's devices as plain entities.
    fn app(automations: Option<&str>) -> Arc<App> {
        let dir = crate::e2e_tests::fixture_dir();
        let cfg = dir.join("config");
        let compat =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/automations");
        for f in ["scripts.yaml", "automations.yaml"] {
            std::fs::copy(compat.join(f), cfg.join(f)).unwrap();
        }
        if let Some(a) = automations {
            std::fs::write(cfg.join("automations.yaml"), a).unwrap();
        }
        std::fs::write(
            cfg.join("configuration.yaml"),
            "automation: !include automations.yaml\nscript: !include scripts.yaml\n",
        )
        .unwrap();
        let app = Arc::new(App::load(&cfg, &dir.join("frontend"), false).unwrap());
        for (id, st, name) in [
            ("light.demo_device_ceiling", "off", "Demo Device Ceiling"),
            ("light.demo_device_desk", "off", "Demo Device Desk"),
            ("switch.demo_device_fan", "off", "Demo Device Fan"),
            (
                "binary_sensor.demo_device_front_door",
                "off",
                "Demo Device Front door",
            ),
            (
                "sensor.demo_device_temperature",
                "21.5",
                "Demo Device Temperature",
            ),
        ] {
            set(&app, id, st, name, Context::new(Some(USER.into())));
        }
        app
    }

    fn set(app: &App, id: &str, state: &str, name: &str, ctx: Context) {
        let attrs = json!({"friendly_name": name}).as_object().unwrap().clone();
        app.states.set(id, state, attrs, ctx);
    }

    /// Let spawned runs finish (the paused clock skips their delays).
    async fn settle() {
        tokio::time::sleep(Duration::from_secs(5)).await;
    }

    /// The recording's result for the first `ty` command sent for `domain`.
    fn reply(ty: &str, domain: &str) -> Value {
        let rec = recording();
        let id = &rec
            .iter()
            .find(|(d, m)| d == "send" && m["type"] == ty && m["domain"] == domain)
            .unwrap()
            .1["id"];
        rec.iter()
            .find(|(d, m)| d == "recv" && &m["id"] == id && m["type"] == "result")
            .unwrap()
            .1["result"]
            .clone()
    }

    /// The recorded `subscribe_entities` changes of one entity, and its initial state.
    fn recorded_changes(entity: &str) -> (Value, Vec<Value>) {
        let events: Vec<Value> = recording()
            .into_iter()
            .filter(|(d, m)| d == "recv" && m["type"] == "event")
            .map(|(_, m)| m["event"].clone())
            .collect();
        let initial = events
            .iter()
            .find_map(|e| e["a"].get(entity))
            .unwrap()
            .clone();
        let changes = events
            .iter()
            .filter_map(|e| e["c"].get(entity).cloned())
            .collect();
        (initial, changes)
    }

    /// The fan automation as recorded: the fan turning on runs it (door closed); the
    /// `automation.trigger` call and the fan turning on again with the door open stop at the
    /// condition. Its entity and traces must come out as HA's did.
    #[tokio::test(start_paused = true)]
    async fn fan_automation_runs_like_ha() {
        let app = app(None);
        start(&app);
        let (initial, changes) = recorded_changes("automation.fan_lights_ceiling");
        let state = app.states.get("automation.fan_lights_ceiling").unwrap();
        assert_eq!(norm(&state.compressed()), initial, "initial state");
        let (mut rx, _) = app.states.subscribe();

        let user = || Context::new(Some(USER.into()));
        set(
            &app,
            "switch.demo_device_fan",
            "on",
            "Demo Device Fan",
            user(),
        );
        settle().await;
        let light = app.states.get("light.demo_device_ceiling").unwrap();
        assert_eq!(light.state, "on");
        assert!(
            light.context.parent_id.is_some(),
            "the automation's context"
        );

        set(
            &app,
            "switch.demo_device_fan",
            "off",
            "Demo Device Fan",
            user(),
        );
        set(
            &app,
            "binary_sensor.demo_device_front_door",
            "on",
            "Demo Device Front door",
            user(),
        );
        let out = crate::services::dispatch(
            &app,
            "automation",
            "trigger",
            json!({"entity_id": "automation.fan_lights_ceiling", "skip_condition": false}),
            json!({}),
            user(),
            vec![],
        )
        .await
        .unwrap();
        assert_eq!(out.child_run.as_ref().unwrap().0, "automation");
        set(
            &app,
            "switch.demo_device_fan",
            "on",
            "Demo Device Fan",
            user(),
        );
        settle().await;

        let mut got = vec![];
        while let Ok(c) = rx.try_recv() {
            if &*c.entity_id == "automation.fan_lights_ceiling" {
                got.push(norm(&c.diff()["c"]["automation.fan_lights_ceiling"]));
            }
        }
        assert_eq!(got, changes, "state changes");
        let list = app.traces.list("automation", Some("1700000000001"));
        assert_eq!(
            norm(&json!(list)),
            reply("trace/list", "automation"),
            "trace/list"
        );
        let last = list.last().unwrap()["run_id"].as_str().unwrap().to_string();
        assert_eq!(
            norm(
                &app.traces
                    .get("automation", "1700000000001", &last)
                    .unwrap()
            ),
            reply("trace/get", "automation"),
            "trace/get"
        );
    }

    /// The MQTT button runs `script.blink` from the queued automation, as recorded.
    #[tokio::test(start_paused = true)]
    async fn mqtt_trigger_runs_the_script() {
        let app = app(None);
        let (_, changes) = recorded_changes("automation.button_blinks_desk");
        let (mut rx, _) = app.states.subscribe();
        let msg = |payload: &str| crate::mqtt::Message {
            topic: "lh/button".into(),
            payload: payload.into(),
            qos: 0,
        };
        on_mqtt(&app, &app.automations.all()[1], 0, &msg("release")); // not the wanted payload
        on_mqtt(&app, &app.automations.all()[1], 0, &msg("press"));
        settle().await;
        let mut got = vec![];
        while let Ok(c) = rx.try_recv() {
            if &*c.entity_id == "automation.button_blinks_desk" {
                got.push(norm(&c.diff()["c"]["automation.button_blinks_desk"]));
            }
        }
        assert_eq!(got, changes, "state changes");
        let list = app.traces.list("automation", Some("1700000000002"));
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["trigger"], "mqtt topic lh/button");
        assert_eq!(list[0]["script_execution"], "finished");
        let t = app
            .traces
            .get(
                "automation",
                "1700000000002",
                list[0]["run_id"].as_str().unwrap(),
            )
            .unwrap();
        let trig = &t["trace"]["trigger/0"][0]["changed_variables"]["trigger"];
        assert_eq!(
            *trig,
            json!({"id": "0", "idx": "0", "alias": null, "platform": "mqtt", "topic": "lh/button",
                "payload": "press", "qos": 0, "description": "mqtt topic lh/button"})
        );
        assert_eq!(t["trace"]["action/0"][0]["child_id"]["domain"], "script");
        // blink's choose: the fan is off, so the default branch turned it on
        assert_eq!(
            app.states.get("switch.demo_device_fan").unwrap().state,
            "on"
        );
    }

    /// The nightly automation fires at 03:00 local time (a paused clock runs through the
    /// night), and not once turned off, as the recording turns it off.
    #[tokio::test(start_paused = true)]
    async fn time_trigger_and_turn_off() {
        let app = app(None);
        set(
            &app,
            "light.demo_device_desk",
            "on",
            "Demo Device Desk",
            Context::new(None),
        );
        start(&app);
        // Any 24 h holds exactly one 03:00 (UTC: no DST), whenever the test runs.
        tokio::time::sleep(Duration::from_secs(24 * 3600)).await;
        let list = app.traces.list("automation", Some("1700000000003"));
        assert_eq!(list.len(), 1, "one night, one run");
        assert_eq!(list[0]["trigger"], "time");
        assert_eq!(list[0]["script_execution"], "finished");
        assert_eq!(
            app.states.get("light.demo_device_desk").unwrap().state,
            "off"
        );
        let t = app
            .traces
            .get(
                "automation",
                "1700000000003",
                list[0]["run_id"].as_str().unwrap(),
            )
            .unwrap();
        let now = t["trace"]["trigger/0"][0]["changed_variables"]["trigger"]["now"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            now.contains("T03:00:00.") && now.ends_with("+00:00"),
            "{now}"
        );
        assert_eq!(
            t["trace"]["condition/0"][0]["result"],
            json!({"result": true, "entities": ["sensor.demo_device_temperature"]})
        );

        let (_, changes) = recorded_changes("automation.nightly_off");
        let (mut rx, _) = app.states.subscribe();
        let user = Context::new(Some(USER.into()));
        let off = json!({"entity_id": "automation.nightly_off"});
        crate::services::dispatch(&app, "automation", "turn_off", off, json!({}), user, vec![])
            .await
            .unwrap();
        let c = rx.try_recv().unwrap();
        assert_eq!(norm(&c.diff()["c"]["automation.nightly_off"]), changes[0]);
        tokio::time::sleep(Duration::from_secs(25 * 3600)).await;
        assert_eq!(
            app.traces.list("automation", Some("1700000000003")).len(),
            1
        );
    }

    #[tokio::test(start_paused = true)]
    async fn state_trigger_for_and_trigger_condition() {
        let app = app(Some(
            r#"
- id: "door"
  alias: Door open long
  triggers:
    - trigger: state
      entity_id: binary_sensor.demo_device_front_door
      to: "on"
      for: { seconds: 5 }
      id: open
    - trigger: state
      entity_id: switch.demo_device_fan
  conditions:
    - condition: trigger
      id: open
  actions:
    - action: light.turn_on
      target: { entity_id: light.demo_device_desk }
"#,
        ));
        start(&app);
        let door = |s: &str| {
            set(
                &app,
                "binary_sensor.demo_device_front_door",
                s,
                "Door",
                Context::new(None),
            )
        };
        let runs = || app.traces.list("automation", Some("door"));
        door("on");
        tokio::time::sleep(Duration::from_secs(3)).await;
        door("off"); // closed before 5 s: nothing
        tokio::time::sleep(Duration::from_secs(10)).await;
        assert!(runs().is_empty());
        door("on");
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(runs().len(), 1);
        assert_eq!(runs()[0]["script_execution"], "finished");
        assert_eq!(
            app.states.get("light.demo_device_desk").unwrap().state,
            "on"
        );
        let t = app
            .traces
            .get("automation", "door", runs()[0]["run_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(
            t["trace"]["trigger/0"][0]["changed_variables"]["trigger"]["for"],
            json!({"__type": "<class 'datetime.timedelta'>", "total_seconds": 5.0})
        );

        // The fan's trigger (any change, attributes too) fires, but its id isn't "open".
        set(
            &app,
            "switch.demo_device_fan",
            "off",
            "Fan renamed",
            Context::new(None),
        );
        settle().await;
        assert_eq!(runs().len(), 2);
        assert_eq!(runs()[1]["script_execution"], "failed_conditions");
    }

    #[test]
    fn parsing() {
        let p = |v: Value| parse_trigger(0, &v);
        // HA rejects a YAML bool as a state: `to: on` must be quoted
        assert!(p(json!({"trigger": "state", "entity_id": "a.b", "to": true})).is_err());
        assert!(
            p(json!({"trigger": "state", "entity_id": "a.b", "attribute": "x", "to": 5})).is_ok()
        );
        assert!(p(json!({"trigger": "sun", "event": "sunset"})).is_err());
        let t = p(json!({"platform": "time", "at": ["03:00", "04:05:06"], "id": 7})).unwrap();
        assert_eq!(t.id, "7");
        assert!(matches!(t.kind, Kind::Time { ref at, .. } if at.len() == 2));

        let reg = crate::registry::Registry::default();
        let states = States::default();
        let cfg = [
            json!({"alias": "Bad", "triggers": [{"trigger": "sun"}], "actions": []}),
            json!({"triggers": [], "actions": [], "initial_state": false}),
        ];
        Automations::load(
            &cfg,
            std::path::Path::new("/nonexistent"),
            &reg,
            &states,
            &crate::restore::Restore::load(std::path::Path::new("/nonexistent")),
        );
        let bad = states.get("automation.bad").unwrap();
        assert_eq!(bad.state, "unavailable");
        let unnamed = states.get("automation.automation_1").unwrap();
        assert_eq!(unnamed.state, "off");
        assert!(unnamed.attributes.get("id").is_none());
    }

    #[test]
    fn next_times() {
        let utc = crate::config::Zone::utc();
        let t = |s: &str| s.parse::<DateTime<Utc>>().unwrap();
        let at = NaiveTime::from_hms_opt(3, 0, 0).unwrap();
        assert_eq!(
            next_time(&utc, t("2026-11-25T02:59:59Z"), at),
            t("2026-11-25T03:00:00Z")
        );
        assert_eq!(
            next_time(&utc, t("2026-11-25T03:00:00Z"), at),
            t("2026-11-26T03:00:00Z")
        );
        let ams = crate::config::Zone::load("Europe/Amsterdam").unwrap();
        // 03:00 in Amsterdam is 02:00 UTC in winter
        assert_eq!(
            next_time(&ams, t("2026-11-25T12:00:00Z"), at),
            t("2026-11-26T02:00:00Z")
        );
    }

    #[tokio::test(start_paused = true)]
    async fn pr2_triggers_replay_ha_recording() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../compat/fixtures/step2/pr2-triggers-conditions.json"
        ))
        .unwrap();

        // 1. numeric_state, template, homeassistant start & shutdown
        let app = app(Some(
            r#"
- id: "ns_above_20"
  triggers:
    - trigger: numeric_state
      entity_id: sensor.temp
      above: 20
  actions: []
- id: "ns_range_10_30_attr"
  triggers:
    - trigger: numeric_state
      entity_id: sensor.hum
      attribute: level
      above: 15
      below: 25
  actions: []
- id: "ns_value_template"
  triggers:
    - trigger: numeric_state
      entity_id: sensor.temp
      value_template: "{{ float(state.state) * 2 }}"
      above: 50
  actions: []
- id: "tmpl_both_on"
  triggers:
    - trigger: template
      value_template: "{{ is_state('input_boolean.a', 'on') and is_state('input_boolean.b', 'on') }}"
  actions: []
- id: "ha_start"
  triggers:
    - trigger: homeassistant
      event: start
  actions: []
- id: "ha_shutdown"
  triggers:
    - trigger: homeassistant
      event: shutdown
  actions: []
"#,
        ));
        let set_raw = |id: &str, st: &str, attrs: Value| {
            app.states.set(
                id,
                st,
                attrs.as_object().unwrap().clone(),
                Context::new(None),
            );
        };
        set_raw("sensor.temp", "15", json!({}));
        set_raw("sensor.hum", "50", json!({"level": 10}));
        set_raw("input_boolean.a", "off", json!({}));
        set_raw("input_boolean.b", "off", json!({}));

        start(&app);
        settle().await;

        let ns_steps = [
            ("sensor.temp", "19", json!({})),
            ("sensor.temp", "20", json!({})),
            ("sensor.temp", "21", json!({})),
            ("sensor.temp", "26", json!({})),
            ("sensor.temp", "unavailable", json!({})),
            ("sensor.temp", "22", json!({})),
            ("sensor.hum", "50", json!({"level": 20})),
            ("sensor.hum", "50", json!({"level": 25})),
            ("sensor.hum", "50", json!({"level": 22})),
        ];
        for (eid, st, attrs) in ns_steps {
            set_raw(eid, st, attrs);
            settle().await;
        }

        for (eid, st) in [
            ("input_boolean.a", "on"),
            ("input_boolean.b", "on"),
            ("input_boolean.a", "on"),
            ("input_boolean.b", "off"),
            ("input_boolean.b", "on"),
        ] {
            set_raw(eid, st, json!({}));
            settle().await;
        }

        stop(&app).await;
        settle().await;

        let trigger_vars = |id: &str| -> Vec<Value> {
            app.traces
                .list("automation", Some(id))
                .into_iter()
                .map(|item| {
                    let t = app
                        .traces
                        .get("automation", id, item["run_id"].as_str().unwrap())
                        .unwrap();
                    t["trace"]["trigger/0"][0]["changed_variables"]["trigger"].clone()
                })
                .collect()
        };

        let expected = fixture["trigger_events"].as_array().unwrap();
        let expected_for = |name: &str| -> Vec<Value> {
            expected
                .iter()
                .filter(|e| e["name"] == name)
                .map(|e| e["trigger"].clone())
                .collect()
        };

        let strip_state_meta = |v: &mut Value| {
            for key in ["from_state", "to_state"] {
                if let Some(obj) = v.get_mut(key).and_then(Value::as_object_mut) {
                    let eid = obj.get("entity_id").cloned().unwrap_or(Value::Null);
                    let st = obj.get("state").cloned().unwrap_or(Value::Null);
                    let attrs = obj.get("attributes").cloned().unwrap_or(json!({}));
                    *obj = json!({"entity_id": eid, "state": st, "attributes": attrs})
                        .as_object()
                        .unwrap()
                        .clone();
                }
            }
        };

        for name in [
            "ns_above_20",
            "ns_range_10_30_attr",
            "ns_value_template",
            "tmpl_both_on",
            "ha_start",
            "ha_shutdown",
        ] {
            let mut got = trigger_vars(name);
            for g in &mut got {
                strip_state_meta(g);
            }
            assert_eq!(got, expected_for(name), "trigger events for {name}");
        }

        // 2. time_pattern matching against HA's recorded sample times
        let sample_times = [
            "2026-11-24T00:00:00+00:00",
            "2026-11-24T00:00:20+00:00",
            "2026-11-24T00:15:00+00:00",
            "2026-11-24T01:30:00+00:00",
            "2026-11-24T02:30:00+00:00",
            "2026-11-24T02:30:01+00:00",
        ];
        let utc = crate::config::Zone::utc();
        for (cfg_str, expected_matches) in fixture["time_pattern_matches"].as_object().unwrap() {
            let mut cfg: Value = serde_json::from_str(cfg_str).unwrap();
            cfg["trigger"] = json!("time_pattern");
            let t = parse_trigger(0, &cfg).unwrap();
            let Kind::TimePattern {
                hours,
                minutes,
                seconds,
            } = t.kind
            else {
                panic!("expected TimePattern");
            };
            let got: Vec<&str> = sample_times
                .iter()
                .copied()
                .filter(|ts| {
                    let dt = ts.parse::<DateTime<Utc>>().unwrap();
                    find_time_pattern_at_or_after(&utc, dt, &hours, &minutes, &seconds) == dt
                })
                .collect();
            let want: Vec<&str> = expected_matches
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap())
                .collect();
            assert_eq!(got, want, "time_pattern {cfg_str}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn time_pattern_fires_and_numeric_for() {
        let app = app(Some(
            r#"
- id: "tp"
  triggers:
    - trigger: time_pattern
      seconds: "/20"
  actions: []
- id: "ns_for"
  triggers:
    - trigger: numeric_state
      entity_id: sensor.temp
      above: 25
      for: { seconds: 5 }
  actions: []
"#,
        ));
        app.states
            .set("sensor.temp", "20", Map::new(), Context::new(None));
        start(&app);
        tokio::task::yield_now().await;
        // 60 s has 3 * 20 s ticks
        tokio::time::sleep(Duration::from_secs(60)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert_eq!(app.traces.list("automation", Some("tp")).len(), 3);

        app.states
            .set("sensor.temp", "26", Map::new(), Context::new(None));
        tokio::time::sleep(Duration::from_secs(3)).await;
        app.states
            .set("sensor.temp", "24", Map::new(), Context::new(None));
        tokio::time::sleep(Duration::from_secs(5)).await;
        assert!(app.traces.list("automation", Some("ns_for")).is_empty());

        app.states
            .set("sensor.temp", "27", Map::new(), Context::new(None));
        tokio::time::sleep(Duration::from_secs(6)).await;
        assert_eq!(app.traces.list("automation", Some("ns_for")).len(), 1);
    }
}
