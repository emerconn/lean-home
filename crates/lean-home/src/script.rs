//! Scripts: HA's script engine (`helpers/script.py`) and the `script` integration
//! (`components/script/`). Each run is a task that executes the sequence step by step, records a
//! trace as it goes (`trace.rs`), and keeps the entity's state up to date. An automation's actions
//! run here too (`automation.rs`): its entity is a `ScriptEntity` in the `automation` domain.

use std::fmt::Write as _;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::sync::watch;

use crate::condition::{Condition, Eval, entity_ids};
use crate::services::{Outcome, dispatch};
use crate::state::{Context, States, iso};
use crate::trace::{ElementRef, Recorder};
use crate::{App, template};

/// HA's `CONTEXT_RECENT_TIME_SECONDS`: state writes reuse the caller's context this long.
const CONTEXT_RECENT: Duration = Duration::from_secs(5);
/// HA's `DEFAULT_MAX` runs for queued and parallel scripts.
const DEFAULT_MAX: usize = 10;

type Seq = Arc<Vec<Step>>;

#[derive(Debug)]
struct Step {
    alias: Option<String>,
    enabled: Option<Value>,
    continue_on_error: bool,
    action: Action,
}

#[derive(Debug)]
enum Action {
    Call {
        action: String,
        data: Value,
        target: Value,
    },
    Delay(Value),
    Choose {
        options: Vec<(Vec<Condition>, Seq)>,
        default: Option<Seq>,
    },
    If {
        conditions: Vec<Condition>,
        then: Seq,
        otherwise: Option<Seq>,
    },
    Condition {
        kind: String,
        condition: Condition,
    },
    Variables(Map<String, Value>),
    Stop {
        message: String,
        error: bool,
    },
}

fn parse_conditions(v: Option<&Value>) -> Result<Vec<Condition>, String> {
    match v {
        Some(Value::Array(a)) => a.iter().map(Condition::parse).collect(),
        Some(c) => Ok(vec![Condition::parse(c)?]),
        None => Ok(vec![]),
    }
}

/// A sequence: a list of actions, or a single one.
fn parse_sequence(v: &Value) -> Result<Seq, String> {
    let steps = match v {
        Value::Array(a) => a.iter().map(parse_step).collect::<Result<_, _>>()?,
        Value::Null => vec![],
        single => vec![parse_step(single)?],
    };
    Ok(Arc::new(steps))
}

/// One action, by its key (HA's `cv.determine_script_action`).
fn parse_step(v: &Value) -> Result<Step, String> {
    let m = v.as_object().ok_or("expected an action")?;
    let action = if let Some(a) = m.get("action").or(m.get("service")) {
        let mut target = m.get("target").cloned().unwrap_or_else(|| json!({}));
        // The legacy top-level entity_id is a target.
        if let Some(e) = m.get("entity_id") {
            target["entity_id"] = e.clone();
        }
        let mut data = m.get("data").cloned().unwrap_or_else(|| json!({}));
        if let (Some(d), Some(Value::Object(extra))) =
            (data.as_object_mut(), m.get("data_template"))
        {
            d.extend(extra.clone());
        }
        Action::Call {
            action: a.as_str().ok_or("action: expected a string")?.into(),
            data,
            target,
        }
    } else if let Some(d) = m.get("delay") {
        Action::Delay(d.clone())
    } else if let Some(c) = m.get("choose") {
        let options = match c {
            Value::Array(a) => a.clone(),
            single => vec![single.clone()],
        };
        Action::Choose {
            options: options
                .iter()
                .map(|o| {
                    Ok((
                        parse_conditions(o.get("conditions"))?,
                        parse_sequence(o.get("sequence").unwrap_or(&Value::Null))?,
                    ))
                })
                .collect::<Result<_, String>>()?,
            default: m.get("default").map(parse_sequence).transpose()?,
        }
    } else if let Some(c) = m.get("if") {
        Action::If {
            conditions: parse_conditions(Some(c))?,
            then: parse_sequence(m.get("then").unwrap_or(&Value::Null))?,
            otherwise: m.get("else").map(parse_sequence).transpose()?,
        }
    } else if let Some(kind) = m.get("condition") {
        let cond: Map<String, Value> = m
            .iter()
            .filter(|(k, _)| !matches!(k.as_str(), "alias" | "enabled" | "continue_on_error"))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Action::Condition {
            kind: kind.as_str().unwrap_or("condition").into(),
            condition: Condition::parse(&Value::Object(cond))?,
        }
    } else if let Some(Value::Object(vars)) = m.get("variables") {
        Action::Variables(vars.clone())
    } else if let Some(stop) = m.get("stop") {
        Action::Stop {
            message: stop.as_str().unwrap_or_default().into(),
            error: m.get("error").and_then(Value::as_bool).unwrap_or(false),
        }
    } else {
        // shortcut: wait_template, wait_for_trigger, repeat, parallel, sequence, event, scene
        // and device actions aren't implemented; a script using one is unavailable, as HA
        // marks an invalid script.
        let keys: Vec<&str> = m.keys().map(String::as_str).collect();
        return Err(format!("unsupported action: {}", keys.join(", ")));
    };
    Ok(Step {
        alias: m.get("alias").and_then(Value::as_str).map(String::from),
        enabled: m.get("enabled").cloned(),
        continue_on_error: m
            .get("continue_on_error")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        action,
    })
}

/// Render every template in a value, as HA's `render_complex` (results parsed).
pub(crate) fn render_complex(
    states: &Arc<States>,
    v: &Value,
    vars: &Map<String, Value>,
) -> Result<Value, String> {
    Ok(match v {
        Value::String(s) if template::is_static(s) => v.clone(),
        Value::String(s) => {
            let (text, _) = template::render(states, s, &Value::Object(vars.clone()), false)?;
            template::result(s, &text)
        }
        Value::Array(a) => a
            .iter()
            .map(|x| render_complex(states, x, vars))
            .collect::<Result<_, _>>()?,
        Value::Object(m) => {
            let mut out = Map::new();
            for (k, x) in m {
                let key = match render_complex(states, &Value::String(k.clone()), vars)? {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                out.insert(key, render_complex(states, x, vars)?);
            }
            out.into()
        }
        _ => v.clone(),
    })
}

/// HA's `cv.positive_time_period`: seconds, `HH:MM[:SS[.f]]`, or `{days, hours, minutes,
/// seconds, milliseconds}`.
pub(crate) fn time_period(v: &Value) -> Result<f64, String> {
    let bad = || format!("offset {v} should be format 'HH:MM', 'HH:MM:SS' or 'HH:MM:SS.F'");
    let secs = match v {
        Value::Number(n) => n.as_f64().ok_or_else(bad)?,
        Value::String(s) if s.contains(':') => {
            let (neg, s) = s
                .strip_prefix('-')
                .map_or((false, s.as_str()), |r| (true, r));
            let parts: Vec<f64> = s
                .split(':')
                .map(|p| p.trim().parse::<f64>())
                .collect::<Result<_, _>>()
                .map_err(|_| bad())?;
            let total = match parts[..] {
                [h, m] => h * 3600.0 + m * 60.0,
                [h, m, sec] => h * 3600.0 + m * 60.0 + sec,
                _ => return Err(bad()),
            };
            if neg { -total } else { total }
        }
        Value::String(s) => s.trim().parse::<f64>().map_err(|_| bad())?,
        Value::Object(m) => {
            let f = |k: &str| {
                m.get(k)
                    .and_then(|x| {
                        x.as_f64()
                            .or_else(|| x.as_str().and_then(|s| s.parse().ok()))
                    })
                    .unwrap_or(0.0)
            };
            f("days") * 86400.0
                + f("hours") * 3600.0
                + f("minutes") * 60.0
                + f("seconds")
                + f("milliseconds") / 1000.0
        }
        _ => return Err(bad()),
    };
    if secs < 0.0 {
        return Err("Time period must be positive".into());
    }
    Ok(secs)
}

/// Python's `str(timedelta(seconds=s))`: `0:00:01`, `0:00:01.500000`, `1 day, 0:00:00`.
fn py_timedelta(secs: f64) -> String {
    let micros = (secs * 1e6).round() as i64;
    let (days, rest) = (micros / 86_400_000_000, micros % 86_400_000_000);
    let (h, m, s, us) = (
        rest / 3_600_000_000,
        rest / 60_000_000 % 60,
        rest / 1_000_000 % 60,
        rest % 1_000_000,
    );
    let mut out = String::new();
    if days != 0 {
        let _ = write!(out, "{days} day{}, ", if days == 1 { "" } else { "s" });
    }
    let _ = write!(out, "{h}:{m:02}:{s:02}");
    if us != 0 {
        let _ = write!(out, ".{us:06}");
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Mode {
    Single,
    Restart,
    Queued,
    Parallel,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Self::Single => "single",
            Self::Restart => "restart",
            Self::Queued => "queued",
            Self::Parallel => "parallel",
        }
    }
}

struct Slot {
    id: u64,
    stop: watch::Sender<bool>,
    done: watch::Receiver<bool>,
}

#[derive(Default)]
struct Runtime {
    runs: Vec<Slot>,
    next_id: u64,
    last_triggered: Option<DateTime<Utc>>,
    last_action: Option<String>,
    context: Option<(Context, Instant)>,
    /// An automation's on/off state (scripts are "on" while running instead).
    enabled: bool,
}

/// A `script.*` entity (HA's `ScriptEntity`), or an `automation.*` one (HA's
/// `AutomationEntity`, whose actions are a script), or an unavailable one for an invalid config.
pub struct ScriptEntity {
    pub(crate) domain: &'static str,
    /// The script's key; an automation's object id. Names it in the log.
    key: String,
    /// What its traces are stored under: the script's key, the automation's `id`.
    pub(crate) item_id: String,
    /// An automation's `id`, shown as the `id` attribute.
    unique_id: Option<String>,
    pub(crate) entity_id: String,
    pub(crate) name: String,
    pub(crate) raw: Value,
    /// Err: the config is invalid, the entity is unavailable.
    sequence: Result<Seq, String>,
    mode: Mode,
    max: usize,
    max_exceeded: String,
    variables: Option<Map<String, Value>>,
    rt: Mutex<Runtime>,
    /// Queued runs wait their turn (tokio's mutex is FIFO).
    queue: tokio::sync::Mutex<()>,
}

impl ScriptEntity {
    fn script(key: &str, raw: &Value, registry: &crate::registry::Registry) -> Self {
        let entity_id = registry
            .entity("script", "script", Some(key))
            .and_then(|e| e["entity_id"].as_str())
            .map_or_else(|| format!("script.{key}"), String::from);
        let name = raw.get("alias").and_then(Value::as_str).unwrap_or(key);
        let sequence = raw.get("sequence").unwrap_or(&Value::Null);
        Self::new(
            "script",
            (key, key, None),
            entity_id,
            name,
            raw,
            sequence,
            Ok(()),
        )
    }

    /// `ids`: (key, trace item id, unique id). `check`: the config's other parts (an
    /// automation's triggers and conditions); an error makes the entity unavailable.
    pub(crate) fn new(
        domain: &'static str,
        (key, item_id, unique_id): (&str, &str, Option<&str>),
        entity_id: String,
        name: &str,
        raw: &Value,
        sequence: &Value,
        check: Result<(), String>,
    ) -> Self {
        let mode = match raw.get("mode").and_then(Value::as_str).unwrap_or("single") {
            "restart" => Ok(Mode::Restart),
            "queued" => Ok(Mode::Queued),
            "parallel" => Ok(Mode::Parallel),
            "single" => Ok(Mode::Single),
            m => Err(format!(
                "value must be one of ['parallel', 'queued', 'restart', 'single'] for dictionary value @ data['mode']: {m}"
            )),
        };
        let sequence = check
            .and(mode.clone())
            .and_then(|_| parse_sequence(sequence));
        if let Err(e) = &sequence {
            eprintln!("{domain} {key}: invalid config, the {domain} is unavailable: {e}");
        }
        Self {
            domain,
            key: key.into(),
            item_id: item_id.into(),
            unique_id: unique_id.map(String::from),
            entity_id,
            name: name.into(),
            raw: raw.clone(),
            sequence,
            mode: mode.unwrap_or(Mode::Single),
            max: raw
                .get("max")
                .and_then(Value::as_u64)
                .map_or(DEFAULT_MAX, |m| m as usize),
            max_exceeded: raw
                .get("max_exceeded")
                .and_then(Value::as_str)
                .unwrap_or("WARNING")
                .to_uppercase(),
            variables: raw.get("variables").and_then(Value::as_object).cloned(),
            rt: Mutex::new(Runtime {
                enabled: raw.get("initial_state").and_then(Value::as_bool) != Some(false),
                ..Runtime::default()
            }),
            queue: tokio::sync::Mutex::new(()),
        }
    }

    fn supports_max(&self) -> bool {
        matches!(self.mode, Mode::Queued | Mode::Parallel)
    }

    /// Write the entity's state (HA's `async_write_ha_state` with `capability_attributes` and
    /// `extra_state_attributes`).
    pub(crate) fn write(&self, states: &States) {
        let mut rt = self.rt.lock().unwrap();
        let mut attrs = Map::new();
        if let Some(id) = &self.unique_id {
            attrs.insert("id".into(), id.clone().into());
        }
        let state = if self.sequence.is_err() {
            "unavailable"
        } else {
            attrs.insert("last_triggered".into(), rt.last_triggered.map(iso).into());
            attrs.insert("mode".into(), self.mode.name().into());
            attrs.insert("current".into(), rt.runs.len().into());
            if self.supports_max() {
                attrs.insert("max".into(), self.max.into());
            }
            match (self.domain, &rt.last_action) {
                ("automation", _) => {}
                (_, Some(a)) => {
                    attrs.insert("last_action".into(), a.clone().into());
                }
                _ => {}
            }
            match self.domain {
                "automation" if rt.enabled => "on",
                "automation" => "off",
                _ if rt.runs.is_empty() => "off",
                _ => "on",
            }
        };
        attrs.insert("friendly_name".into(), self.name.clone().into());
        if let Some(icon) = self.raw.get("icon") {
            attrs.insert("icon".into(), icon.clone());
        }
        // An entity reuses the context it was last given for 5 s, then writes with a new one.
        if rt
            .context
            .as_ref()
            .is_some_and(|(_, t)| t.elapsed() > CONTEXT_RECENT)
        {
            rt.context = None;
        }
        let ctx = rt
            .context
            .as_ref()
            .map_or_else(|| Context::new(None), |(c, _)| c.clone());
        drop(rt);
        states.set(&self.entity_id, state, attrs, ctx);
    }

    /// The state HA writes for an entity removed while its registry entry stays (a reload
    /// replacing or dropping it): unavailable, `restored`.
    pub(crate) fn write_removed(&self, states: &States) {
        let mut attrs = Map::new();
        attrs.insert("restored".into(), true.into());
        if let Some(id) = &self.unique_id {
            attrs.insert("id".into(), id.clone().into());
        }
        attrs.insert("friendly_name".into(), self.name.clone().into());
        attrs.insert("supported_features".into(), 0.into());
        states.set(&self.entity_id, "unavailable", attrs, Context::new(None));
    }

    /// What it takes from its stored state at startup (`restore.rs`): `last_triggered`, and an
    /// automation's on/off unless `initial_state` says.
    pub(crate) fn restore_stored(&self, state: &Value) {
        let (last, on) = crate::restore::parse(state);
        let mut rt = self.rt.lock().unwrap();
        rt.last_triggered = last;
        if let Some(on) = on.filter(|_| {
            self.raw
                .get("initial_state")
                .and_then(Value::as_bool)
                .is_none()
        }) {
            rt.enabled = on;
        }
    }

    /// Restore from the stored states, if this entity has one.
    pub(crate) fn restore_from(
        &self,
        restore: &crate::restore::Restore,
        registry: &crate::registry::Registry,
    ) {
        let reg = registry.entry_id(&self.entity_id);
        if let Some(s) = restore.last_state(&self.entity_id, reg.as_deref()) {
            self.restore_stored(&s);
        }
    }

    /// It's going away: its last state goes into the stored states.
    pub(crate) fn forget(&self, app: &App) {
        let reg = app.registry.entry_id(&self.entity_id);
        app.restore
            .removed(app.states.get(&self.entity_id).as_deref(), reg.as_deref());
    }

    /// What a replacement keeps from the entity it replaces (HA's RestoreEntity reads the last
    /// state): `last_triggered`, and an automation's on/off unless `initial_state` says.
    pub(crate) fn restore(&self, prev: &ScriptEntity) {
        let old = prev.rt.lock().unwrap();
        let mut rt = self.rt.lock().unwrap();
        rt.last_triggered = old.last_triggered;
        if self
            .raw
            .get("initial_state")
            .and_then(Value::as_bool)
            .is_none()
        {
            rt.enabled = old.enabled;
        }
    }

    pub(crate) fn set_context(&self, ctx: &Context) {
        self.rt.lock().unwrap().context = Some((ctx.clone(), Instant::now()));
    }

    pub(crate) fn log(&self, level: &str, msg: &str) {
        eprintln!(
            "{}.{} [{level}] {}: {msg}",
            self.domain, self.key, self.name
        );
    }

    /// A valid config: the entity isn't unavailable.
    pub(crate) fn available(&self) -> bool {
        self.sequence.is_ok()
    }

    pub(crate) fn enabled(&self) -> bool {
        self.rt.lock().unwrap().enabled
    }

    pub(crate) fn set_enabled(&self, on: bool) {
        self.rt.lock().unwrap().enabled = on;
    }

    /// The config's `variables`, rendered into `vars` (HA's `ScriptVariables.async_render`).
    // shortcut: each is rendered against the variables before it; HA's run variables also win
    // over a script's defaults, which only matters when both set the same name.
    pub(crate) fn render_variables(
        &self,
        states: &Arc<States>,
        vars: &mut Map<String, Value>,
    ) -> Result<(), String> {
        for (k, v) in self.variables.iter().flatten() {
            let r = render_complex(states, v, vars)?;
            vars.insert(k.clone(), r);
        }
        Ok(())
    }

    /// Stop every run and wait for them (HA's `async_stop`).
    pub(crate) async fn stop(&self) {
        let waits: Vec<watch::Receiver<bool>> = {
            let rt = self.rt.lock().unwrap();
            rt.runs
                .iter()
                .map(|r| {
                    let _ = r.stop.send(true);
                    r.done.clone()
                })
                .collect()
        };
        for mut w in waits {
            let _ = w.wait_for(|d| *d).await;
        }
    }

    fn is_running(&self) -> bool {
        !self.rt.lock().unwrap().runs.is_empty()
    }
}

/// All scripts; replaced one by one when the config is reloaded.
#[derive(Default)]
pub struct Scripts {
    items: RwLock<IndexMap<String, Arc<ScriptEntity>>>,
}

impl Scripts {
    /// From `script:` in the config; writes each entity's initial state.
    pub fn load(
        config: &Map<String, Value>,
        registry: &crate::registry::Registry,
        states: &States,
        restore: &crate::restore::Restore,
    ) -> Self {
        let items: IndexMap<_, _> = config
            .iter()
            .map(|(key, raw)| {
                (
                    key.clone(),
                    Arc::new(ScriptEntity::script(key, raw, registry)),
                )
            })
            .collect();
        for e in items.values() {
            e.restore_from(restore, registry);
            e.write(states);
        }
        Self {
            items: RwLock::new(items),
        }
    }

    pub fn entity_ids(&self) -> Vec<String> {
        self.items
            .read()
            .unwrap()
            .values()
            .map(|e| e.entity_id.clone())
            .collect()
    }

    fn get(&self, key: &str) -> Option<Arc<ScriptEntity>> {
        self.items.read().unwrap().get(key).cloned()
    }

    /// `script.reload` (HA's `_async_process_config`): re-read the config; a script whose config
    /// didn't change keeps running, a changed one is replaced (keeping `last_triggered`), a new
    /// one is added, a removed one stops and is shown unavailable.
    pub async fn reload(&self, app: &Arc<App>) -> Result<(), String> {
        let config = crate::config::load(&app.config_dir)?;
        let old = self.items.read().unwrap().clone();
        let mut next = IndexMap::new();
        let mut added = vec![];
        for (key, raw) in &config.scripts {
            match old.get(key) {
                Some(e) if e.raw == *raw => {
                    next.insert(key.clone(), e.clone());
                }
                prev => {
                    let e = Arc::new(ScriptEntity::script(key, raw, &app.registry));
                    if let Some(prev) = prev {
                        prev.stop().await;
                        prev.forget(app);
                        prev.write_removed(&app.states);
                        e.restore(prev);
                    } else {
                        e.restore_from(&app.restore, &app.registry);
                    }
                    next.insert(key.clone(), e.clone());
                    added.push(e);
                }
            }
        }
        for (key, e) in &old {
            if !next.contains_key(key) {
                e.stop().await;
                e.forget(app);
                e.write_removed(&app.states);
            }
        }
        *self.items.write().unwrap() = next;
        for e in added {
            e.write(&app.states);
        }
        Ok(())
    }

    /// The config editor deleted a script: its entity goes away (HA removes its registry entry).
    pub async fn remove(&self, app: &App, key: &str) {
        let e = self.items.write().unwrap().shift_remove(key);
        if let Some(e) = e {
            e.stop().await;
            e.forget(app);
            app.states.remove(&e.entity_id);
        }
    }

    /// The `script` domain of `get_services`: each script, then HA's four services.
    pub fn describe(&self) -> Value {
        let mut out = Map::new();
        for e in self
            .items
            .read()
            .unwrap()
            .values()
            .filter(|e| e.sequence.is_ok())
        {
            let fields: Map<String, Value> = e
                .raw
                .get("fields")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .map(|(k, f)| (k.clone(), field(f)))
                .collect();
            out.insert(
                e.key.clone(),
                json!({"name": e.name, "description": e.raw.get("description").and_then(Value::as_str).unwrap_or(""),
                    "fields": fields, "response": {"optional": true}}),
            );
        }
        out.insert("reload".into(), json!({"fields": {}}));
        let target = json!({"entity": [{"domain": ["script"]}]});
        for s in ["turn_on", "turn_off", "toggle"] {
            out.insert(s.into(), json!({"fields": {}, "target": target}));
        }
        out.into()
    }

    fn by_entity_id(&self, id: &str) -> Option<Arc<ScriptEntity>> {
        self.items
            .read()
            .unwrap()
            .values()
            .find(|e| e.entity_id == id)
            .cloned()
    }
}

/// A script field as HA's schema normalizes it: `advanced` and `required` default to false,
/// and an entity selector gets its defaults.
fn field(f: &Value) -> Value {
    let mut f = f.as_object().cloned().unwrap_or_default();
    if let Some(Value::Object(sel)) = f.get_mut("selector")
        && let Some(Value::Object(entity)) = sel.get_mut("entity")
    {
        if let Some(Value::String(d)) = entity.get("domain").cloned() {
            entity.insert("domain".into(), json!([d]));
        }
        entity.entry("multiple").or_insert(json!(false));
        entity.entry("reorder").or_insert(json!(false));
    }
    // shortcut: only the entity selector gets HA's normalized defaults; other selectors are
    // passed as written.
    f.entry("advanced").or_insert(json!(false));
    f.entry("required").or_insert(json!(false));
    f.into()
}

/// The `script` domain's services (HA's `components/script/services.py`, and `script.<key>`).
pub async fn call_service(
    app: &Arc<App>,
    service: &str,
    data: Value,
    target: Value,
    ctx: Context,
    stack: Vec<String>,
) -> Result<Outcome, (&'static str, String)> {
    let scripts = &app.scripts;
    if let Some(e) = scripts.get(service).filter(|e| e.sequence.is_ok()) {
        let vars = data.as_object().cloned().unwrap_or_default();
        let run_id = start(app, e.clone(), vars, ctx, true, stack).await;
        return Ok(Outcome {
            child_run: run_id.map(|r| ("script", e.key.clone(), r)),
        });
    }
    if !matches!(service, "turn_on" | "turn_off" | "toggle") {
        if service == "reload" {
            return scripts
                .reload(app)
                .await
                .map(|_| Outcome::default())
                .map_err(|e| ("home_assistant_error", e));
        }
        return Err(("not_found", format!("Service script.{service} not found.")));
    }
    let ids: Vec<String> = entity_ids(&data["entity_id"])
        .into_iter()
        .chain(entity_ids(&target["entity_id"]))
        .collect();
    let mut child = None;
    for id in ids {
        let Some(e) = scripts.by_entity_id(&id).filter(|e| e.sequence.is_ok()) else {
            continue;
        };
        let vars = data["variables"].as_object().cloned().unwrap_or_default();
        match service {
            "turn_on" => {
                child = start(app, e.clone(), vars, ctx.clone(), false, stack.clone())
                    .await
                    .map(|r| ("script", e.key.clone(), r))
            }
            "turn_off" => {
                e.set_context(&ctx);
                e.stop().await;
            }
            _ if e.is_running() => e.stop().await,
            _ => {
                child = start(
                    app,
                    e.clone(),
                    Map::new(),
                    ctx.clone(),
                    false,
                    stack.clone(),
                )
                .await
                .map(|r| ("script", e.key.clone(), r))
            }
        }
    }
    Ok(Outcome { child_run: child })
}

/// A run's variables: a script's are built when it starts (`this`, its `variables`, the
/// caller's); an automation's arrive ready, built with its trigger before the conditions.
pub(crate) enum Vars {
    Run(Map<String, Value>),
    Ready(Map<String, Value>),
}

type Started = Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>;

/// Start a script run (HA's `_async_start_run`). `wait`: return when it finishes
/// (`script.<key>`), else once it has started (`script.turn_on`). Returns the run's trace id,
/// also for a run its mode refused.
fn start(
    app: &Arc<App>,
    entity: Arc<ScriptEntity>,
    run_vars: Map<String, Value>,
    ctx: Context,
    wait: bool,
    stack: Vec<String>,
) -> Started {
    let trace = app.traces.start(
        entity.domain,
        &entity.item_id,
        entity.raw.clone(),
        ctx.clone(),
    );
    launch(app, entity, trace, Vars::Run(run_vars), ctx, wait, stack)
}

/// Run the entity's sequence into `trace`, as its mode allows (HA's `Script.async_run`).
pub(crate) fn launch(
    app: &Arc<App>,
    entity: Arc<ScriptEntity>,
    trace: Recorder,
    vars: Vars,
    ctx: Context,
    wait: bool,
    stack: Vec<String>,
) -> Started {
    let app = app.clone();
    Box::pin(async move {
        let run_id = trace.run_id();
        let refuse = |exec: &str, level: &str, msg: &str| {
            if entity.max_exceeded != "SILENT" {
                entity.log(level, msg);
            }
            trace.finish(exec, None);
            Some(run_id.clone())
        };
        let (running, runs) = {
            let rt = entity.rt.lock().unwrap();
            (!rt.runs.is_empty(), rt.runs.len())
        };
        if running && entity.mode == Mode::Single {
            return refuse("failed_single", &entity.max_exceeded, "Already running");
        }
        if running && entity.mode != Mode::Restart && runs >= entity.max {
            return refuse(
                "failed_max_runs",
                &entity.max_exceeded,
                "Maximum number of runs exceeded",
            );
        }
        if matches!(entity.mode, Mode::Restart | Mode::Queued) && stack.contains(&entity.entity_id)
        {
            entity.log("WARNING", &format!("Disallowed recursion detected, tried to start {} which is already running in the current execution path", entity.entity_id));
            trace.finish("disallowed_recursion_detected", None);
            return Some(run_id);
        }

        // A script's variables: `this` (the state before the run), its own, the caller's.
        let mut vars = match vars {
            Vars::Ready(vars) => vars,
            Vars::Run(run_vars) => {
                let mut vars = Map::new();
                vars.insert(
                    "this".into(),
                    app.states
                        .get(&entity.entity_id)
                        .map_or(Value::Null, |s| s.json()),
                );
                if let Err(e) = entity.render_variables(&app.states, &mut vars) {
                    entity.log("ERROR", &format!("Error rendering variables: {e}"));
                    trace.finish("error", Some(e));
                    return Some(run_id);
                }
                vars.extend(run_vars);
                vars
            }
        };
        vars.insert("context".into(), ctx.json());

        let (stop_tx, stop_rx) = watch::channel(false);
        let (done_tx, done_rx) = watch::channel(false);
        let (id, others) = {
            let mut rt = entity.rt.lock().unwrap();
            rt.next_id += 1;
            let id = rt.next_id;
            let others: Vec<_> = rt
                .runs
                .iter()
                .map(|r| (r.stop.clone(), r.done.clone()))
                .collect();
            rt.runs.push(Slot {
                id,
                stop: stop_tx,
                done: done_rx.clone(),
            });
            (id, others)
        };
        if entity.mode == Mode::Restart && !others.is_empty() {
            entity.log("INFO", "Restarting");
            for (stop, mut done) in others {
                let _ = stop.send(true);
                let _ = done.wait_for(|d| *d).await;
            }
        }
        entity.set_context(&ctx);
        entity.rt.lock().unwrap().last_triggered = Some(Utc::now());
        entity.write(&app.states);

        let task = {
            let (app, entity) = (app.clone(), entity.clone());
            let mut stack = stack;
            stack.push(entity.entity_id.clone());
            async move {
                let _turn = if entity.mode == Mode::Queued {
                    Some(entity.queue.lock().await)
                } else {
                    None
                };
                let Ok(seq) = entity.sequence.clone() else {
                    return;
                };
                let mut run = Run {
                    app: app.clone(),
                    entity: entity.clone(),
                    ctx,
                    trace: trace.clone(),
                    stop: stop_rx,
                    stack,
                    levels: vec![],
                };
                let base = if entity.domain == "automation" {
                    "action"
                } else {
                    "sequence"
                };
                let r = run.run_sequence(seq, base.into(), &mut vars).await;
                let (exec, err) = match r {
                    Ok(()) | Err(Halt::Stop) => ("finished", None),
                    Err(Halt::Abort(_)) | Err(Halt::ConditionFail) => ("aborted", None),
                    Err(Halt::Cancelled) => ("cancelled", None),
                    Err(Halt::Error(e)) => ("error", Some(e)),
                };
                // HA's `_finish`
                {
                    let mut rt = entity.rt.lock().unwrap();
                    rt.runs.retain(|r| r.id != id);
                    if rt.runs.is_empty() {
                        rt.last_action = None;
                    }
                }
                entity.write(&app.states);
                trace.finish(exec, err);
                let _ = done_tx.send(true);
            }
        };
        let handle = tokio::spawn(task);
        if wait {
            let _ = handle.await;
        }
        Some(run_id)
    })
}

/// Why a sequence stopped before its end.
#[derive(Debug)]
enum Halt {
    /// A `stop` action: the run finished.
    Stop,
    /// A `stop` with `error: true`.
    Abort(String),
    /// A `condition` action was false.
    ConditionFail,
    Cancelled,
    Error(String),
}

type Running<'a> = Pin<Box<dyn Future<Output = Result<(), Halt>> + Send + 'a>>;

/// One run of a script (HA's `_ScriptRun`).
struct Run {
    app: Arc<App>,
    entity: Arc<ScriptEntity>,
    ctx: Context,
    trace: Recorder,
    stop: watch::Receiver<bool>,
    stack: Vec<String>,
    /// The `last_action` of each sub-sequence being run (`choose`/`if` branches are sub-scripts
    /// in HA, which pass their last action up to the script when they change).
    levels: Vec<Option<String>>,
}

impl Run {
    /// HA's `_step_log`: the action shown as `last_action` (written on the next change).
    fn step_log(&mut self, step: &Step, default: String) {
        let msg = step.alias.clone().unwrap_or(default);
        match self.levels.last_mut() {
            Some(level) => *level = Some(msg),
            None => self.entity.rt.lock().unwrap().last_action = Some(msg),
        }
    }

    /// HA's `_changed`: a sub-sequence's last action becomes the script's, then the state is
    /// written.
    fn changed(&self) {
        if let Some(level) = self.levels.last() {
            self.entity.rt.lock().unwrap().last_action = level.clone();
        }
        self.entity.write(&self.app.states);
    }

    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }

    fn run_sequence<'a>(
        &'a mut self,
        steps: Seq,
        base: String,
        vars: &'a mut Map<String, Value>,
    ) -> Running<'a> {
        Box::pin(async move {
            for (i, step) in steps.iter().enumerate() {
                if self.stopped() {
                    return Err(Halt::Cancelled);
                }
                let path = format!("{base}/{i}");
                let el = self.trace.element(&path, Some(vars));
                if let Some(en) = &step.enabled {
                    let on = match render_complex(&self.app.states, en, vars) {
                        Ok(Value::Bool(b)) => b,
                        Ok(v) => v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")),
                        Err(e) => return Err(self.fail(&el, step, i, e)),
                    };
                    if !on {
                        self.trace.set_result(&el, json!({"enabled": false}));
                        continue;
                    }
                }
                match self.exec(step, &path, &el, vars).await {
                    Ok(()) => {}
                    Err(Halt::Error(e)) => {
                        let h = self.fail(&el, step, i, e);
                        if step.continue_on_error {
                            continue;
                        }
                        return Err(h);
                    }
                    Err(Halt::Abort(m)) => {
                        self.trace.set_error(&el, &m);
                        return Err(Halt::Abort(m));
                    }
                    Err(other) => return Err(other),
                }
            }
            Ok(())
        })
    }

    /// A step failed: trace and log it (HA's `_log_exception`).
    // shortcut: with continue_on_error every step error is skipped; HA still stops on
    // configuration errors and non-HA exceptions.
    fn fail(&self, el: &ElementRef, step: &Step, i: usize, e: String) -> Halt {
        self.trace.set_error(el, &e);
        if !step.continue_on_error {
            self.entity.log(
                "ERROR",
                &format!(
                    "Error executing script. Error for {} at pos {}: {e}",
                    step.kind(),
                    i + 1
                ),
            );
        }
        Halt::Error(e)
    }

    /// A `choose` option's or `if`'s conditions (HA's `_test_conditions`): traced at `path`,
    /// each at `path/sub/i`. None when one errored.
    fn test_conditions(
        &self,
        conds: &[Condition],
        path: &str,
        sub: &str,
        vars: &Map<String, Value>,
    ) -> Option<bool> {
        let el = self.trace.element(path, Some(vars));
        let ev = Eval {
            states: &self.app.states,
            vars,
            trace: Some(&self.trace),
        };
        let mut result = Some(true);
        for (i, c) in conds.iter().enumerate() {
            match c.check(&ev, &format!("{path}/{sub}/{i}"), None) {
                Ok(true) => {}
                Ok(false) => {
                    result = Some(false);
                    break;
                }
                Err(e) => {
                    self.entity
                        .log("WARNING", &format!("Error in '{sub}[{i}]' evaluation: {e}"));
                    result = None;
                    break;
                }
            }
        }
        self.trace.update_result(&el, json!({"result": result}));
        result
    }

    /// A `choose`/`if` branch, as HA runs it: a sub-script whose start changes the script.
    async fn run_sub(
        &mut self,
        seq: Seq,
        base: String,
        vars: &Map<String, Value>,
    ) -> Result<(), Halt> {
        self.levels.push(None);
        self.changed();
        // shortcut: variables set in a branch stay in the branch; HA's scoping also lets a
        // branch update variables the script defined before it.
        let mut sub_vars = vars.clone();
        let r = self.run_sequence(seq, base, &mut sub_vars).await;
        self.levels.pop();
        match r {
            // A failed condition ends only the branch (HA doesn't re-raise it from a sub-script).
            Err(Halt::ConditionFail) => Ok(()),
            other => other,
        }
    }

    async fn exec(
        &mut self,
        step: &Step,
        path: &str,
        el: &ElementRef,
        vars: &mut Map<String, Value>,
    ) -> Result<(), Halt> {
        let states = self.app.states.clone();
        let render = |v: &Value, vars: &Map<String, Value>| {
            render_complex(&states, v, vars).map_err(Halt::Error)
        };
        match &step.action {
            Action::Call {
                action,
                data,
                target,
            } => {
                self.step_log(step, "call service".into());
                let action = match render(&Value::String(action.clone()), vars)? {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                let (domain, service) = action
                    .split_once('.')
                    .ok_or_else(|| Halt::Error(format!("Invalid action '{action}'")))?;
                let data = render(data, vars)?;
                let mut target = render(target, vars)?;
                // cv.comp_entity_ids: the trace (and the call) see a list
                for key in ["entity_id", "device_id", "area_id", "floor_id", "label_id"] {
                    if let Some(v) = target.get(key).cloned() {
                        target[key] = if key == "entity_id" && v != "all" {
                            json!(entity_ids(&v))
                        } else if v.is_string() {
                            json!([v])
                        } else {
                            v
                        };
                    }
                }
                let running_script = domain == "script"
                    || domain == "python_script"
                    || (domain, service) == ("automation", "trigger");
                self.trace.set_result(el, json!({"params": {"domain": domain, "service": service, "service_data": data, "target": target}, "running_script": running_script}));
                let out = dispatch(
                    &self.app,
                    domain,
                    service,
                    data,
                    target,
                    self.ctx.clone(),
                    self.stack.clone(),
                )
                .await
                .map_err(|(_, m)| Halt::Error(m))?;
                if let Some((domain, item, run)) = out.child_run {
                    self.trace.set_child(el, domain, &item, &run);
                }
                Ok(())
            }
            Action::Delay(d) => {
                let secs = time_period(&render(d, vars)?).map_err(Halt::Error)?;
                self.step_log(step, format!("delay {}", py_timedelta(secs)));
                self.changed();
                self.trace
                    .set_result(el, json!({"delay": secs, "done": secs == 0.0}));
                if secs == 0.0 {
                    return Ok(());
                }
                let mut stop = self.stop.clone();
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs_f64(secs)) => {
                        self.trace.set_result(el, json!({"delay": secs, "done": true}));
                        Ok(())
                    }
                    _ = stop.wait_for(|s| *s) => Err(Halt::Cancelled),
                }
            }
            Action::Choose { options, default } => {
                for (idx, (conds, seq)) in options.iter().enumerate() {
                    let p = format!("{path}/choose/{idx}");
                    if self.test_conditions(conds, &p, "conditions", vars) == Some(true) {
                        self.trace.set_result(el, json!({"choice": idx}));
                        return self
                            .run_sub(seq.clone(), format!("{p}/sequence"), vars)
                            .await;
                    }
                }
                if let Some(def) = default {
                    self.trace.set_result(el, json!({"choice": "default"}));
                    return self
                        .run_sub(def.clone(), format!("{path}/default"), vars)
                        .await;
                }
                Ok(())
            }
            Action::If {
                conditions,
                then,
                otherwise,
            } => {
                if self.test_conditions(conditions, &format!("{path}/if"), "condition", vars)
                    == Some(true)
                {
                    self.trace.set_result(el, json!({"choice": "then"}));
                    return self
                        .run_sub(then.clone(), format!("{path}/then"), vars)
                        .await;
                }
                if let Some(o) = otherwise {
                    self.trace.set_result(el, json!({"choice": "else"}));
                    return self.run_sub(o.clone(), format!("{path}/else"), vars).await;
                }
                Ok(())
            }
            Action::Condition { kind, condition } => {
                let label = step.alias.clone().unwrap_or_else(|| kind.clone());
                match self.levels.last_mut() {
                    Some(level) => *level = Some(label),
                    None => self.entity.rt.lock().unwrap().last_action = Some(label),
                }
                let ev = Eval {
                    states: &self.app.states,
                    vars,
                    trace: Some(&self.trace),
                };
                let ok = match condition.check(&ev, path, Some(el.clone())) {
                    Ok(b) => b,
                    Err(e) => {
                        self.entity
                            .log("WARNING", &format!("Error in 'condition' evaluation:\n{e}"));
                        false
                    }
                };
                self.trace.update_result(el, json!({"result": ok}));
                if ok { Ok(()) } else { Err(Halt::ConditionFail) }
            }
            Action::Variables(defs) => {
                self.step_log(step, "assigning variables".into());
                for (k, v) in defs {
                    let r = render(v, vars)?;
                    vars.insert(k.clone(), r);
                }
                Ok(())
            }
            Action::Stop { message, error } => {
                self.trace
                    .set_result(el, json!({"stop": message, "error": error}));
                if *error {
                    self.entity
                        .log("ERROR", &format!("Error script sequence: {message}"));
                    Err(Halt::Abort(message.clone()))
                } else {
                    self.entity
                        .log("INFO", &format!("Stop script sequence: {message}"));
                    Err(Halt::Stop)
                }
            }
        }
    }
}

impl Step {
    fn kind(&self) -> &'static str {
        match self.action {
            Action::Call { .. } => "call_service",
            Action::Delay(_) => "delay",
            Action::Choose { .. } => "choose",
            Action::If { .. } => "if",
            Action::Condition { .. } => "condition",
            Action::Variables(_) => "variables",
            Action::Stop { .. } => "stop",
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The recording's volatile values as its placeholders, and numbers as JavaScript keeps them
    /// (the fixtures went through JSON.parse: `1.0` is `1`).
    pub(crate) fn norm(v: &Value) -> Value {
        match v {
            Value::String(s) => {
                let hex32 = s.len() == 32
                    && s.bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
                let ulid = s.len() == 26
                    && s.bytes()
                        .all(|b| b.is_ascii_digit() || b.is_ascii_uppercase());
                let ts = s.len() >= 19 && s.as_bytes()[4] == b'-' && s.as_bytes()[10] == b'T';
                if hex32 {
                    "<hex32>".into()
                } else if ulid {
                    "<ulid>".into()
                } else if ts {
                    "<ts>".into()
                } else {
                    v.clone()
                }
            }
            Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && !n.is_i64() && !n.is_u64() => json!(f as i64),
                _ => v.clone(),
            },
            Value::Array(a) => a.iter().map(norm).collect(),
            Value::Object(m) => m
                .iter()
                .map(|(k, x)| {
                    // subscribe_entities times are epoch seconds, recorded as 0
                    let x = if matches!(k.as_str(), "lc" | "lu") && x.is_number() {
                        json!(0)
                    } else {
                        norm(x)
                    };
                    (k.clone(), x)
                })
                .collect::<Map<_, _>>()
                .into(),
            _ => v.clone(),
        }
    }

    /// Every WS message of the automations recording, as (direction, message).
    pub(crate) fn recording() -> Vec<(String, Value)> {
        include_str!("../../../compat/fixtures/automations/ws.jsonl")
            .lines()
            .map(|l| serde_json::from_str::<Value>(l).unwrap())
            .flat_map(|l| {
                let dir = l["dir"].as_str().unwrap().to_string();
                match &l["msg"] {
                    Value::Array(a) => a
                        .iter()
                        .map(|m| (dir.clone(), m.clone()))
                        .collect::<Vec<_>>(),
                    m => vec![(dir, m.clone())],
                }
            })
            .collect()
    }

    fn app() -> Arc<App> {
        let dir = crate::e2e_tests::fixture_dir();
        let cfg = dir.join("config");
        let compat =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat/automations");
        std::fs::copy(compat.join("scripts.yaml"), cfg.join("scripts.yaml")).unwrap();
        std::fs::write(
            cfg.join("configuration.yaml"),
            "script: !include scripts.yaml\n",
        )
        .unwrap();
        let app = Arc::new(App::load(&cfg, &dir.join("frontend"), false).unwrap());
        let set = |id: &str, st: &str| app.states.set(id, st, Map::new(), Context::new(None));
        set("light.demo_device_ceiling", "on");
        set("light.demo_device_desk", "off");
        set("switch.demo_device_fan", "on");
        app
    }

    fn blink(
        app: &Arc<App>,
        light: &str,
        ctx: Context,
    ) -> impl Future<Output = Result<Outcome, (&'static str, String)>> {
        dispatch(
            app,
            "script",
            "blink",
            json!({"target_light": light}),
            json!({}),
            ctx,
            vec![],
        )
    }

    /// The two recorded runs of `script.blink`: from the automation (fan on: the `choose`
    /// branch turns the desk light off), then from the frontend (fan off: `default` turns the
    /// fan on). lean-home must change the entity and trace the run exactly as HA did.
    #[tokio::test(start_paused = true)]
    async fn blink_runs_like_ha() {
        let app = app();
        assert_eq!(app.states.get("script.blink").unwrap().state, "off");
        let first = blink(&app, "light.demo_device_desk", Context::new(None))
            .await
            .unwrap();
        assert_eq!(
            app.states.get("light.demo_device_desk").unwrap().state,
            "off"
        );
        assert!(first.child_run.is_some());

        app.states.set(
            "switch.demo_device_fan",
            "off",
            Map::new(),
            Context::new(None),
        );
        let (mut rx, _) = app.states.subscribe();
        let user = Context::new(Some("0123456789abcdef0123456789abcdef".into()));
        let second = blink(&app, "light.demo_device_ceiling", user)
            .await
            .unwrap();
        assert_eq!(
            app.states.get("switch.demo_device_fan").unwrap().state,
            "on"
        );

        let rec = recording();
        // script.blink's diffs in the recording: the second run is the last four.
        let want: Vec<Value> = rec
            .iter()
            .filter(|(d, m)| d == "recv" && m["type"] == "event")
            .filter_map(|(_, m)| m["event"]["c"].get("script.blink").cloned())
            .collect();
        let mut got = vec![];
        while let Ok(change) = rx.try_recv() {
            if &*change.entity_id == "script.blink" {
                got.push(norm(&change.diff()["c"]["script.blink"]));
            }
        }
        assert_eq!(got, want[want.len() - 4..], "script.blink state changes");

        let reply = |ty: &str| {
            let id = &rec
                .iter()
                .find(|(d, m)| d == "send" && m["type"] == ty && m["domain"] == "script")
                .unwrap()
                .1["id"];
            rec.iter()
                .find(|(d, m)| d == "recv" && &m["id"] == id)
                .unwrap()
                .1["result"]
                .clone()
        };
        let run = second.child_run.unwrap().2;
        assert_eq!(
            norm(&app.traces.get("script", "blink", &run).unwrap()),
            reply("trace/get"),
            "trace/get"
        );
        assert_eq!(
            norm(&json!(app.traces.list("script", Some("blink")))),
            reply("trace/list"),
            "trace/list"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn modes_and_services() {
        let app = app();
        let ctx = || Context::new(None);
        // single: a second run while one is running is refused, and traced as such
        let a = {
            let app = app.clone();
            tokio::spawn(
                async move { blink(&app, "light.demo_device_desk", Context::new(None)).await },
            )
        };
        tokio::task::yield_now().await;
        let refused = blink(&app, "light.demo_device_desk", ctx())
            .await
            .unwrap()
            .child_run
            .unwrap()
            .2;
        assert_eq!(
            app.traces.get("script", "blink", &refused).unwrap()["script_execution"],
            "failed_single"
        );
        a.await.unwrap().unwrap();

        // turn_on returns once started; turn_off cancels the run mid-delay
        let target = json!({"entity_id": "script.blink"});
        let started = dispatch(
            &app,
            "script",
            "turn_on",
            json!({"variables": {"target_light": "light.demo_device_desk"}}),
            target.clone(),
            ctx(),
            vec![],
        )
        .await
        .unwrap();
        // Like HA, turn_on returns once the run has written "on"; let it reach its delay.
        assert_eq!(app.states.get("script.blink").unwrap().state, "on");
        tokio::time::sleep(Duration::from_millis(10)).await;
        let s = app.states.get("script.blink").unwrap();
        assert_eq!(
            (s.state.as_str(), &s.attributes["last_action"]),
            ("on", &json!("delay 0:00:01"))
        );
        dispatch(&app, "script", "turn_off", json!({}), target, ctx(), vec![])
            .await
            .unwrap();
        let t = app
            .traces
            .get("script", "blink", &started.child_run.unwrap().2)
            .unwrap();
        assert_eq!(t["script_execution"], "cancelled");
        assert_eq!(
            t["trace"]["sequence/1"][0]["result"],
            json!({"delay": 1.0, "done": false})
        );
        let s = app.states.get("script.blink").unwrap();
        assert_eq!(s.state, "off");
        assert!(s.attributes.get("last_action").is_none());

        assert_eq!(
            dispatch(&app, "script", "nope", json!({}), json!({}), ctx(), vec![])
                .await
                .unwrap_err()
                .0,
            "not_found"
        );
        let services = app.scripts.describe();
        assert_eq!(
            services["blink"]["fields"]["target_light"]["selector"],
            json!({"entity": {"domain": ["light"], "multiple": false, "reorder": false}})
        );
    }

    #[test]
    fn periods_and_timedeltas() {
        assert_eq!(time_period(&json!(1)), Ok(1.0));
        assert_eq!(time_period(&json!("00:01:30")), Ok(90.0));
        assert_eq!(time_period(&json!("01:02")), Ok(3720.0));
        assert_eq!(time_period(&json!("2.5")), Ok(2.5));
        assert_eq!(
            time_period(&json!({"minutes": 1, "milliseconds": 500})),
            Ok(60.5)
        );
        assert!(time_period(&json!("-00:00:01")).is_err());
        assert_eq!(py_timedelta(1.0), "0:00:01");
        assert_eq!(py_timedelta(1.5), "0:00:01.500000");
        assert_eq!(py_timedelta(90061.0), "1 day, 1:01:01");
        assert_eq!(py_timedelta(172800.0), "2 days, 0:00:00");
    }

    #[test]
    fn unsupported_actions_make_the_script_unavailable() {
        let reg = crate::registry::Registry::default();
        let states = States::default();
        let cfg = json!({"bad": {"sequence": [{"wait_template": "{{ true }}"}]}});
        let restore = crate::restore::Restore::load(std::path::Path::new("/nonexistent"));
        let scripts = Scripts::load(cfg.as_object().unwrap(), &reg, &states, &restore);
        assert_eq!(states.get("script.bad").unwrap().state, "unavailable");
        assert!(scripts.describe().get("bad").is_none());
    }
}
