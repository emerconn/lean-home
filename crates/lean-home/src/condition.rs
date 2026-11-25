//! Conditions, as HA's `helpers/condition/` evaluates and traces them: `state`, `numeric_state`,
//! `template`, `trigger`, `and`/`or`/`not`, and the shorthands (a template string, `- and: [...]`).

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::script::{render_complex, time_period};
use crate::state::{States, iso};
use crate::template;
use crate::trace::{ElementRef, Recorder};

#[derive(Debug, Clone)]
pub enum Condition {
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Vec<Condition>),
    State {
        entity_ids: Vec<String>,
        states: Vec<Value>,
        attribute: Option<String>,
        match_any: bool,
        /// `for:`, a duration or a template of one.
        for_: Option<Value>,
    },
    NumericState {
        entity_ids: Vec<String>,
        above: Option<Value>,
        below: Option<Value>,
        attribute: Option<String>,
        value_template: Option<String>,
    },
    Template(String),
    /// The automation's trigger has one of these ids.
    Trigger(Vec<String>),
}

/// `cv.entity_ids`: a list, or a comma-separated string.
pub fn entity_ids(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s
            .split(',')
            .map(|s| s.trim().to_lowercase())
            .filter(|s| !s.is_empty())
            .collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(|s| s.trim().to_lowercase()))
            .collect(),
        _ => vec![],
    }
}

impl Condition {
    pub fn parse(v: &Value) -> Result<Self, String> {
        if let Some(t) = v.as_str() {
            return Ok(Self::Template(t.into()));
        }
        let m = v.as_object().ok_or("expected a condition")?;
        let list = |key: &str| -> Result<Vec<Condition>, String> {
            match m.get(key) {
                Some(Value::Array(a)) => a.iter().map(Self::parse).collect(),
                Some(single) => Ok(vec![Self::parse(single)?]),
                None => Ok(vec![]),
            }
        };
        // `- and: [...]` shorthand
        for (key, make) in [
            ("and", Self::And as fn(_) -> _),
            ("or", Self::Or),
            ("not", Self::Not),
        ] {
            if m.contains_key(key) && !m.contains_key("condition") {
                return Ok(make(list(key)?));
            }
        }
        let kind = m
            .get("condition")
            .and_then(Value::as_str)
            .ok_or("condition: required key missing")?;
        let opt_str = |k: &str| m.get(k).and_then(Value::as_str).map(String::from);
        Ok(match kind {
            "and" => Self::And(list("conditions")?),
            "or" => Self::Or(list("conditions")?),
            "not" => Self::Not(list("conditions")?),
            "state" => {
                let states = match m.get("state") {
                    Some(Value::Array(a)) => a.clone(),
                    Some(s) => vec![s.clone()],
                    None => return Err("state condition: required key 'state' missing".into()),
                };
                Self::State {
                    entity_ids: entity_ids(m.get("entity_id").unwrap_or(&Value::Null)),
                    states,
                    attribute: opt_str("attribute"),
                    match_any: m.get("match").and_then(Value::as_str) == Some("any"),
                    for_: m.get("for").cloned(),
                }
            }
            "numeric_state" => {
                if !m.contains_key("above") && !m.contains_key("below") {
                    return Err(
                        "numeric_state condition: must contain at least one of below, above".into(),
                    );
                }
                Self::NumericState {
                    entity_ids: entity_ids(m.get("entity_id").unwrap_or(&Value::Null)),
                    above: m.get("above").cloned(),
                    below: m.get("below").cloned(),
                    attribute: opt_str("attribute"),
                    value_template: opt_str("value_template"),
                }
            }
            "template" => Self::Template(
                opt_str("value_template")
                    .ok_or("template condition: required key 'value_template' missing")?,
            ),
            "trigger" => Self::Trigger(match m.get("id") {
                Some(Value::Array(a)) => a.iter().map(id_string).collect(),
                Some(id) => vec![id_string(id)],
                None => return Err("trigger condition: required key 'id' missing".into()),
            }),
            // shortcut: time, sun, zone, device… conditions come with the configs that use them.
            other => return Err(format!("condition type '{other}' is not supported yet")),
        })
    }
}

/// What a condition is evaluated against.
pub struct Eval<'a> {
    pub states: &'a Arc<States>,
    pub vars: &'a Map<String, Value>,
    pub trace: Option<&'a Recorder>,
}

impl Eval<'_> {
    fn element(&self, path: &str) -> Option<ElementRef> {
        self.trace.map(|t| t.element(path, Some(self.vars)))
    }

    fn set(&self, at: &Option<ElementRef>, result: Value) {
        if let (Some(t), Some(at)) = (self.trace, at) {
            t.set_result(at, result);
        }
    }

    fn update(&self, at: &Option<ElementRef>, result: Value) {
        if let (Some(t), Some(at)) = (self.trace, at) {
            t.update_result(at, result);
        }
    }

    fn error(&self, at: &Option<ElementRef>, e: &str) {
        if let (Some(t), Some(at)) = (self.trace, at) {
            t.set_error(at, e);
        }
    }
}

impl Condition {
    /// Check, tracing at `path` (HA's `ConditionChecker.async_check`). `reuse`: the element of a
    /// `condition` step, which the condition writes its result into instead of a new one.
    /// `Err` is HA's `ConditionError`: nothing decided and something failed.
    pub fn check(&self, ev: &Eval, path: &str, reuse: Option<ElementRef>) -> Result<bool, String> {
        let at = reuse.or_else(|| ev.element(path));
        let r = self.check_inner(ev, path, &at);
        match &r {
            Ok(b) => ev.update(&at, json!({"result": b})),
            Err(e) => ev.error(&at, e),
        }
        r
    }

    fn check_inner(&self, ev: &Eval, path: &str, at: &Option<ElementRef>) -> Result<bool, String> {
        match self {
            Self::And(cs) | Self::Or(cs) | Self::Not(cs) => {
                let (name, stop_on, stop_value) = match self {
                    Self::And(_) => ("and", false, false),
                    Self::Or(_) => ("or", true, true),
                    _ => ("not", true, false),
                };
                let mut errors = vec![];
                for (i, c) in cs.iter().enumerate() {
                    match c.check(ev, &format!("{path}/conditions/{i}"), None) {
                        Ok(b) if b == stop_on => return Ok(stop_value),
                        Ok(_) => {}
                        Err(e) => errors.push(format!(
                            "In '{name}' (item {} of {}):\n  {e}",
                            i + 1,
                            cs.len()
                        )),
                    }
                }
                if !errors.is_empty() {
                    return Err(errors.join("\n"));
                }
                Ok(!stop_value)
            }
            Self::State {
                entity_ids,
                states,
                attribute,
                match_any,
                for_,
            } => {
                let mut result = !match_any;
                let mut errors = vec![];
                for (i, id) in entity_ids.iter().enumerate() {
                    let el = ev.element(&format!("{path}/entity_id/{i}"));
                    match state(ev, &el, id, states, attribute.as_deref(), for_.as_ref()) {
                        Ok(true) => result = true,
                        Ok(false) if !match_any => return Ok(false),
                        Ok(false) => {}
                        Err(e) => {
                            ev.error(&el, &e);
                            errors.push(format!(
                                "In 'state' (item {} of {}):\n  {e}",
                                i + 1,
                                entity_ids.len()
                            ));
                        }
                    }
                }
                if !errors.is_empty() {
                    return Err(errors.join("\n"));
                }
                Ok(result)
            }
            Self::NumericState {
                entity_ids,
                above,
                below,
                attribute,
                value_template,
            } => {
                let mut errors = vec![];
                for (i, id) in entity_ids.iter().enumerate() {
                    let el = ev.element(&format!("{path}/entity_id/{i}"));
                    match numeric_state(
                        ev,
                        &el,
                        id,
                        above,
                        below,
                        attribute.as_deref(),
                        value_template.as_deref(),
                    ) {
                        Ok(true) => {}
                        Ok(false) => return Ok(false),
                        Err(e) => {
                            ev.error(&el, &e);
                            errors.push(format!(
                                "In 'numeric_state' (item {} of {}):\n  {e}",
                                i + 1,
                                entity_ids.len()
                            ));
                        }
                    }
                }
                if !errors.is_empty() {
                    return Err(errors.join("\n"));
                }
                Ok(true)
            }
            Self::Template(tpl) => {
                let vars = Value::Object(ev.vars.clone());
                let (text, listeners) = template::render(ev.states, tpl, &vars, false)
                    .map_err(|e| format!("In 'template' condition: {e}"))?;
                let result = text.to_lowercase() == "true";
                ev.set(
                    at,
                    json!({"result": result, "entities": listeners.entities}),
                );
                Ok(result)
            }
            Self::Trigger(ids) => Ok(ev.vars.get("trigger").is_some_and(|t| {
                t.get("id")
                    .is_some_and(|id| ids.iter().any(|i| *id == Value::String(i.clone())))
            })),
        }
    }
}

/// `cv.string`: a trigger id written as a number is its text.
pub fn id_string(v: &Value) -> String {
    v.as_str().map_or_else(|| v.to_string(), String::from)
}

/// HA's `condition.state` for one entity.
fn state(
    ev: &Eval,
    at: &Option<ElementRef>,
    entity_id: &str,
    wanted: &[Value],
    attribute: Option<&str>,
    for_: Option<&Value>,
) -> Result<bool, String> {
    let s = ev
        .states
        .get(entity_id)
        .ok_or_else(|| format!("In 'state' condition: unknown entity {entity_id}"))?;
    let value = match attribute {
        Some(a) => match s.attributes.get(a) {
            Some(v) => v.clone(),
            None => {
                ev.set(at, json!({"result": false, "message": format!("attribute '{a}' of entity {entity_id} does not exist")}));
                return Ok(false);
            }
        },
        None => Value::String(s.state.clone()),
    };
    let mut is_state = false;
    let mut wanted_value = Value::Null;
    for w in wanted {
        wanted_value = w.clone();
        // A wanted state naming an input entity (`input_select.x`) means that entity's state.
        if let Some(id) = w
            .as_str()
            .filter(|id| id.starts_with("input_") && id.contains('.'))
        {
            let other = ev.states.get(id).ok_or_else(|| {
                format!("In 'state' condition: the 'state' entity {id} is unavailable")
            })?;
            wanted_value = Value::String(other.state.clone());
        }
        is_state = value == wanted_value;
        if is_state {
            break;
        }
    }
    let Some(for_) = for_.filter(|_| is_state) else {
        ev.set(
            at,
            json!({"result": is_state, "state": value, "wanted_state": wanted_value}),
        );
        return Ok(is_state);
    };
    let secs = render_complex(ev.states, for_, ev.vars)
        .map_err(|e| format!("In 'state' condition: template error: {e}"))
        .and_then(|v| {
            time_period(&v).map_err(|e| format!("In 'state' condition: schema error: {e}"))
        })?;
    // HA traces the instant the state must have changed before, as `duration`.
    let since = chrono::Utc::now() - chrono::Duration::microseconds((secs * 1e6) as i64);
    let ok = since > s.last_changed;
    ev.set(
        at,
        json!({"result": ok, "state": value, "duration": iso(since)}),
    );
    Ok(ok)
}

/// HA's `condition.async_numeric_state` for one entity.
fn numeric_state(
    ev: &Eval,
    at: &Option<ElementRef>,
    entity_id: &str,
    above: &Option<Value>,
    below: &Option<Value>,
    attribute: Option<&str>,
    value_template: Option<&str>,
) -> Result<bool, String> {
    let err = |m: String| format!("In 'numeric_state' condition: {m}");
    let s = ev
        .states
        .get(entity_id)
        .ok_or_else(|| err(format!("unknown entity {entity_id}")))?;
    if let Some(a) = attribute
        && !s.attributes.contains_key(a)
    {
        ev.set(at, json!({"result": false, "message": format!("attribute '{a}' of entity {entity_id} does not exist")}));
        return Ok(false);
    }
    let value: Value = match value_template {
        Some(tpl) => {
            let mut vars = ev.vars.clone();
            vars.insert("state".into(), s.json());
            let (text, _) = template::render(ev.states, tpl, &Value::Object(vars), false)
                .map_err(|e| err(format!("template error: {e}")))?;
            template::result(tpl, &text)
        }
        None => match attribute {
            Some(a) => s.attributes.get(a).cloned().unwrap_or(Value::Null),
            None => Value::String(s.state.clone()),
        },
    };
    if value.is_null() || value == "unavailable" || value == "unknown" {
        let shown = value
            .as_str()
            .map_or_else(|| "None".to_string(), String::from);
        ev.set(at, json!({"result": false, "message": format!("value '{shown}' is non-numeric and treated as False")}));
        return Ok(false);
    }
    let fvalue = match &value {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse::<f64>().ok(),
        Value::Bool(b) => Some(*b as u8 as f64),
        _ => None,
    }
    .ok_or_else(|| {
        err(format!(
            "entity {entity_id} state '{}' cannot be processed as a number",
            value.as_str().unwrap_or(&value.to_string())
        ))
    })?;
    // A bound is a number, or an entity whose state is the bound.
    let bound = |b: &Value, which: &str| -> Result<Option<f64>, String> {
        match b {
            Value::Number(n) => Ok(n.as_f64()),
            Value::String(id) => {
                let s = ev
                    .states
                    .get(id)
                    .ok_or_else(|| err(format!("unknown '{which}' entity {id}")))?;
                if s.state == "unavailable" || s.state == "unknown" {
                    return Ok(None);
                }
                s.state.parse().map(Some).map_err(|_| {
                    err(format!(
                        "the '{which}' entity {id} state '{}' cannot be processed as a number",
                        s.state
                    ))
                })
            }
            _ => Ok(None),
        }
    };
    if let Some(b) = below {
        match bound(b, "below")? {
            None => return Ok(false),
            Some(b) if fvalue >= b => {
                ev.set(
                    at,
                    json!({"result": false, "state": fvalue, "wanted_state_below": b}),
                );
                return Ok(false);
            }
            _ => {}
        }
    }
    if let Some(a) = above {
        match bound(a, "above")? {
            None => return Ok(false),
            Some(a) if fvalue <= a => {
                ev.set(
                    at,
                    json!({"result": false, "state": fvalue, "wanted_state_above": a}),
                );
                return Ok(false);
            }
            _ => {}
        }
    }
    ev.set(at, json!({"result": true, "state": fvalue}));
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;
    use crate::trace::Traces;

    fn states() -> Arc<States> {
        let s = Arc::new(States::default());
        let set = |id: &str, st: &str, a: Value| {
            s.set(id, st, a.as_object().unwrap().clone(), Context::new(None))
        };
        set("switch.fan", "off", json!({}));
        set("sensor.t", "21.5", json!({"unit": "C"}));
        set("sensor.limit", "22", json!({}));
        set("binary_sensor.door", "unavailable", json!({}));
        s
    }

    fn check(cond: Value) -> (Result<bool, String>, Value) {
        let s = states();
        let traces = Traces::default();
        let rec = traces.start("script", "t", json!({}), Context::new(None));
        let run = rec.run_id();
        let vars = Map::new();
        let ev = Eval {
            states: &s,
            vars: &vars,
            trace: Some(&rec),
        };
        let r = Condition::parse(&cond).unwrap().check(&ev, "c", None);
        rec.finish("finished", None);
        (r, traces.get("script", "t", &run).unwrap()["trace"].clone())
    }

    #[test]
    fn state_condition_traces_like_ha() {
        // The recorded blink choose: entity fan "off", wanted "on".
        let (r, t) = check(json!({"condition": "state", "entity_id": "switch.fan", "state": "on"}));
        assert_eq!(r, Ok(false));
        assert_eq!(t["c"][0]["result"], json!({"result": false}));
        assert_eq!(
            t["c/entity_id/0"][0]["result"],
            json!({"result": false, "state": "off", "wanted_state": "on"})
        );
        let (r, _) = check(
            json!({"condition": "state", "entity_id": ["switch.fan", "sensor.t"], "state": ["off", "21.5"]}),
        );
        assert_eq!(r, Ok(true));
        let (r, _) = check(
            json!({"condition": "state", "entity_id": "switch.fan, sensor.t", "state": "off", "match": "any"}),
        );
        assert_eq!(r, Ok(true));
        let (r, t) =
            check(json!({"condition": "state", "entity_id": "switch.nope", "state": "on"}));
        assert!(r.unwrap_err().contains("unknown entity switch.nope"));
        assert!(t["c/entity_id/0"][0]["error"].is_string());
    }

    #[test]
    fn numeric_template_and_compound() {
        let (r, t) = check(
            json!({"condition": "numeric_state", "entity_id": "sensor.t", "above": 21, "below": "sensor.limit"}),
        );
        assert_eq!(r, Ok(true));
        assert_eq!(
            t["c/entity_id/0"][0]["result"],
            json!({"result": true, "state": 21.5})
        );
        let (r, t) =
            check(json!({"condition": "numeric_state", "entity_id": "sensor.t", "above": 22}));
        assert_eq!(r, Ok(false));
        assert_eq!(
            t["c/entity_id/0"][0]["result"],
            json!({"result": false, "state": 21.5, "wanted_state_above": 22.0})
        );
        let (r, _) = check(
            json!({"condition": "numeric_state", "entity_id": "binary_sensor.door", "below": 1}),
        );
        assert_eq!(r, Ok(false));
        let (r, t) = check(json!("{{ is_state('switch.fan', 'off') }}"));
        assert_eq!(r, Ok(true));
        assert_eq!(
            t["c"][0]["result"],
            json!({"result": true, "entities": ["switch.fan"]})
        );
        let (r, t) = check(json!({"condition": "or", "conditions": [
            {"condition": "state", "entity_id": "switch.fan", "state": "on"},
            {"condition": "template", "value_template": "{{ true }}"}]}));
        assert_eq!(r, Ok(true));
        assert_eq!(
            t["c/conditions/1"][0]["result"],
            json!({"result": true, "entities": []})
        );
        let (r, _) = check(
            json!({"not": [{"condition": "state", "entity_id": "switch.fan", "state": "on"}]}),
        );
        assert_eq!(r, Ok(true));
        assert!(Condition::parse(&json!({"condition": "sun", "after": "sunset"})).is_err());
    }

    #[test]
    fn trigger_and_state_for() {
        let s = states();
        let mut vars = Map::new();
        vars.insert("trigger".into(), json!({"id": "door", "idx": "0"}));
        let ev = Eval {
            states: &s,
            vars: &vars,
            trace: None,
        };
        let check = |c: Value| Condition::parse(&c).unwrap().check(&ev, "c", None);
        assert_eq!(
            check(json!({"condition": "trigger", "id": "door"})),
            Ok(true)
        );
        assert_eq!(
            check(json!({"condition": "trigger", "id": ["a", "door"]})),
            Ok(true)
        );
        assert_eq!(check(json!({"condition": "trigger", "id": "a"})), Ok(false));
        // switch.fan has been "off" for a moment, not for an hour
        let fan = |f: Value| json!({"condition": "state", "entity_id": "switch.fan", "state": "off", "for": f});
        assert_eq!(check(fan(json!({"hours": 1}))), Ok(false));
        assert_eq!(check(fan(json!(0))), Ok(true));
        assert_eq!(check(fan(json!("{{ 0 }}"))), Ok(true));
        assert!(
            check(fan(json!("nope")))
                .unwrap_err()
                .contains("schema error")
        );
    }
}
