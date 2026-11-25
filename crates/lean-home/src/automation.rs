//! Automations: HA's `automation` integration (`components/automation/`) and the triggers it
//! listens with (`state`, `mqtt` and `time`, from `components/homeassistant/triggers/` and
//! `components/mqtt/trigger.py`). An automation's entity is a `ScriptEntity` whose actions
//! `script.rs` runs; this module adds the triggers and conditions in front of a run.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use chrono::{DateTime, Datelike, NaiveTime, Utc};
use serde_json::{Map, Value, json};
use tokio::sync::broadcast::error::RecvError;
use tokio::task::AbortHandle;

use crate::condition::{Condition, Eval, entity_ids, id_string};
use crate::mqtt::{slugify, topic_matches};
use crate::script::{ScriptEntity, Vars, launch, render_complex, time_period};
use crate::services::Outcome;
use crate::state::{Change, Context, States};
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

const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

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
            let weekdays = m.get("weekday").map(|w| {
                list(Some(w))
                    .iter()
                    .filter_map(|d| d.as_str().map(String::from))
                    .collect()
            });
            Kind::Time { at, weekdays }
        }
        // shortcut: the other platforms (event, numeric_state, template, sun, homeassistant…)
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

pub struct Automation {
    entity: Arc<ScriptEntity>,
    triggers: Vec<Trigger>,
    conditions: Vec<Condition>,
}

/// All automations, and the `for:` timers their state triggers have running.
#[derive(Default)]
pub struct Automations {
    items: Vec<Arc<Automation>>,
    /// (automation index, trigger index, entity) -> the timer waiting out its `for:`.
    pending: Mutex<HashMap<(usize, usize, String), AbortHandle>>,
}

impl Automations {
    /// From `automation:` in the config; writes each entity's initial state.
    pub fn load(config: &[Value], registry: &crate::registry::Registry, states: &States) -> Self {
        let mut items = vec![];
        let mut taken: Vec<String> = vec![];
        for (n, raw) in config.iter().enumerate() {
            let m = raw.as_object().cloned().unwrap_or_default();
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
                    while taken.contains(&eid) || registry.entity_id_taken(&eid) {
                        eid = format!("{base}_{k}");
                        k += 1;
                    }
                    eid
                });
            taken.push(entity_id.clone());
            let triggers = list(plural(&m, "triggers"))
                .iter()
                .enumerate()
                .map(|(i, t)| parse_trigger(i, t))
                .collect::<Result<Vec<_>, _>>();
            let conditions = list(plural(&m, "conditions"))
                .iter()
                .map(Condition::parse)
                .collect::<Result<Vec<_>, _>>();
            let check = match (&triggers, &conditions) {
                (Err(e), _) | (_, Err(e)) => Err(e.clone()),
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
                raw,
                plural(&m, "actions").unwrap_or(&Value::Null),
                check,
            );
            entity.write(states);
            items.push(Arc::new(Automation {
                entity: Arc::new(entity),
                triggers: triggers.unwrap_or_default(),
                conditions: conditions.unwrap_or_default(),
            }));
        }
        // shortcut: on/off and last_triggered aren't restored across restarts (HA's
        // RestoreEntity); add with the recorder step.
        Self {
            items,
            pending: Mutex::default(),
        }
    }

    /// Stop the `for:` timers of automation `n` (it was turned off).
    fn cancel_pending(&self, n: usize) {
        self.pending.lock().unwrap().retain(|(a, _, _), h| {
            if *a == n {
                h.abort();
            }
            *a != n
        });
    }
}

/// Arm every automation's triggers (HA does it once started). Call after MQTT has started.
pub fn start(app: &Arc<App>) {
    let autos = &app.automations.items;
    // Trigger-level `enabled`, rendered once, as HA does when it attaches the triggers.
    let armed = |t: &Trigger, a: &Automation| match &t.enabled {
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
    };
    let mut any_state = false;
    for (n, a) in autos.iter().enumerate() {
        if !a.entity.available() {
            continue;
        }
        for (ti, t) in a.triggers.iter().enumerate() {
            if !armed(t, a) {
                continue;
            }
            match &t.kind {
                Kind::State { .. } => any_state = true,
                Kind::Mqtt { topic, qos, .. } => {
                    let Some(mqtt) = app.mqtt.get() else {
                        a.entity.log("ERROR", "MQTT trigger: MQTT is not set up");
                        continue;
                    };
                    let mut rx = mqtt.subscribe_for(topic, *qos);
                    let app = app.clone();
                    tokio::spawn(async move {
                        loop {
                            match rx.recv().await {
                                Ok(msg) => on_mqtt(&app, n, ti, &msg),
                                Err(RecvError::Lagged(k)) => {
                                    eprintln!("automation: {k} MQTT messages dropped (too slow)")
                                }
                                Err(RecvError::Closed) => return,
                            }
                        }
                    });
                }
                Kind::Time { at, .. } => {
                    for &at in at {
                        tokio::spawn(time_trigger(app.clone(), n, ti, at));
                    }
                }
            }
        }
    }
    if any_state {
        let (mut rx, _) = app.states.subscribe();
        let app = app.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(change) => on_state(&app, &change),
                    Err(RecvError::Lagged(k)) => {
                        eprintln!("automation: {k} state changes dropped (too slow)")
                    }
                    Err(RecvError::Closed) => return,
                }
            }
        });
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

/// A trigger fired: run automation `n` in the background, if it's on.
fn fire(app: &Arc<App>, n: usize, t: &Trigger, data: Map<String, Value>, ctx: Option<Context>) {
    let a = app.automations.items[n].clone();
    if !a.entity.enabled() {
        return;
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
                    return a
                        .entity
                        .log("ERROR", &format!("Error rendering trigger variables: {e}"));
                }
            }
        }
    }
    run_vars.insert("trigger".into(), data.into());
    let app = app.clone();
    tokio::spawn(async move { trigger(&app, &a, run_vars, ctx.as_ref(), false).await });
}

/// HA's `state_automation_listener`, for every automation's state triggers.
fn on_state(app: &Arc<App>, change: &Change) {
    for (n, a) in app.automations.items.iter().enumerate() {
        if !a.entity.available() || !a.entity.enabled() {
            continue;
        }
        for t in &a.triggers {
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
                continue;
            };
            if !entity_ids.iter().any(|e| **e == *change.entity_id) {
                continue;
            }
            let value = |s: &Option<Arc<crate::state::State>>| match (s, attribute) {
                (None, _) => Value::Null,
                (Some(s), None) => Value::String(s.state.clone()),
                (Some(s), Some(a)) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
            };
            let (old, new) = (value(&change.old), value(&change.new));
            if attribute.is_some() && old == new {
                continue;
            }
            if !from.test(&old) || !to.test(&new) || (!match_all && old == new) {
                continue;
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
                fire(app, n, t, data, Some(change.context.clone()));
                continue;
            };
            // `for:`: fire once the entity has kept the matched state that long.
            let mut scope = attach_vars(app, a);
            scope.insert(
                "trigger".into(),
                json!({"platform": "state", "entity_id": &*change.entity_id,
                    "from_state": data["from_state"], "to_state": data["to_state"]}),
            );
            let secs = match render_complex(&app.states, for_, &scope).and_then(|v| time_period(&v))
            {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("Error rendering '{}' for template: {e}", a.entity.name);
                    continue;
                }
            };
            // HA's JSON encoder writes a timedelta this way (templates see the dict too).
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
            let (app2, entity, ctx) = (
                app.clone(),
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
                let t = &app2.automations.items[n].triggers[ti];
                fire(&app2, n, t, data, Some(ctx));
            });
            let key = (n, t.idx, change.entity_id.to_string());
            if let Some(old) = app
                .automations
                .pending
                .lock()
                .unwrap()
                .insert(key, handle.abort_handle())
            {
                old.abort();
            }
        }
    }
}

/// HA's `mqtt_automation_listener`.
fn on_mqtt(app: &Arc<App>, n: usize, ti: usize, msg: &crate::mqtt::Message) {
    let a = &app.automations.items[n];
    let t = &a.triggers[ti];
    let Kind::Mqtt {
        topic,
        payload,
        value_template,
        ..
    } = &t.kind
    else {
        return;
    };
    if !topic_matches(topic, &msg.topic) {
        return;
    }
    let json_payload = serde_json::from_str::<Value>(&msg.payload).ok();
    if let Some(wanted) = payload {
        // The wanted payload is a template rendered with the attach variables.
        let wanted = match template::is_static(wanted) {
            true => wanted.clone(),
            false => {
                match template::render(&app.states, wanted, &attach_vars(app, a).into(), false) {
                    Ok((s, _)) => s,
                    Err(e) => return a.entity.log("ERROR", &format!("payload template: {e}")),
                }
            }
        };
        let got = match value_template {
            None => msg.payload.clone(),
            Some(vt) => {
                let mut vars = json!({"value": msg.payload});
                if let Some(j) = &json_payload {
                    vars["value_json"] = j.clone();
                }
                match template::render(&app.states, vt, &vars, false) {
                    Ok((s, _)) => s,
                    Err(e) => return a.entity.log("ERROR", &format!("value_template: {e}")),
                }
            }
        };
        if got.is_empty() || got != wanted {
            return;
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
    fire(app, n, t, data, None);
}

/// The first instant after `after` whose wall-clock time in `tz` is `at`.
fn next_time(tz: &crate::config::Zone, after: DateTime<Utc>, at: NaiveTime) -> DateTime<Utc> {
    let date = (after + tz.offset(after)).date_naive();
    (0..3)
        .map(|d| tz.local_to_utc((date + chrono::Days::new(d)).and_time(at)))
        .find(|t| *t > after)
        .expect("a wall-clock time recurs within 3 days")
}

/// A time trigger's timer (HA's `async_track_time_change`), firing at `at` local time daily.
async fn time_trigger(app: Arc<App>, n: usize, ti: usize, at: NaiveTime) {
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
        let a = &app.automations.items[n];
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
        fire(&app, n, t, data, None);
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
        // shortcut: reloading re-reads configuration.yaml; restart lean-home instead for now.
        "reload" => {
            return Err((
                "home_assistant_error",
                "Reloading automations is not supported yet".into(),
            ));
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
    for (n, a) in autos.items.iter().enumerate() {
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
                let run = trigger(app, a, vars, Some(&ctx), skip).await;
                child = run.map(|r| ("automation", e.item_id.clone(), r));
            }
            _ if on => {
                e.set_enabled(true);
                e.write(&app.states);
            }
            _ => {
                e.set_enabled(false);
                autos.cancel_pending(n);
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
        on_mqtt(&app, 1, 0, &msg("release")); // not the wanted payload
        on_mqtt(&app, 1, 0, &msg("press"));
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
        tokio::time::sleep(Duration::from_secs(25 * 3600)).await;
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
        Automations::load(&cfg, &reg, &states);
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
}
