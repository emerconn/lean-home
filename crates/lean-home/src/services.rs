//! Services and the demo entities that roadmap step 0 calls for. Real integrations replace the demo.

use std::sync::Arc;

use serde_json::{Map, Value, json};

use crate::App;
use crate::state::{Context, States};

const TOGGLEABLE: [&str; 2] = ["light", "switch"];
const ACTIONS: [&str; 3] = ["turn_on", "turn_off", "toggle"];

pub fn load_demo(states: &States) {
    let ctx = || Context::new(None);
    states.set("light.kitchen", "off", light_attrs("off"), ctx());
    states.set(
        "switch.fan",
        "off",
        attrs(json!({"friendly_name": "Fan"})),
        ctx(),
    );
    states.set(
        "sensor.temperature",
        "21.5",
        attrs(json!({"state_class": "measurement", "unit_of_measurement": "°C", "device_class": "temperature", "friendly_name": "Temperature"})),
        ctx(),
    );
}

fn attrs(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap_or_default()
}

fn light_attrs(state: &str) -> Map<String, Value> {
    let mode = (state == "on").then_some("onoff");
    attrs(
        json!({"supported_color_modes": ["onoff"], "color_mode": mode, "friendly_name": "Kitchen", "supported_features": 0}),
    )
}

/// `get_services`: `{domain: {service: description}}`, with the scripts' services.
pub fn describe(app: &App) -> Value {
    let mut out = Map::new();
    for domain in TOGGLEABLE.iter().chain(&["homeassistant"]) {
        let target = if *domain == "homeassistant" {
            json!({})
        } else {
            json!({"entity": [{"domain": [domain]}]})
        };
        let services: Map<_, _> = ACTIONS
            .iter()
            .map(|a| (a.to_string(), json!({"fields": {}, "target": target})))
            .collect();
        out.insert(domain.to_string(), services.into());
    }
    out.insert("script".into(), app.scripts.describe());
    out.into()
}

/// What a service call started, for the caller's trace: a script run as (item id, run id).
#[derive(Debug, Default)]
pub struct Outcome {
    pub child_run: Option<(String, String)>,
}

/// Any service call, from the WebSocket or a script step. `stack` is the chain of scripts this
/// call comes from (HA's `script_stack_cv`), to refuse a recursion that would deadlock.
pub async fn dispatch(
    app: &Arc<App>,
    domain: &str,
    service: &str,
    data: Value,
    target: Value,
    ctx: Context,
    stack: Vec<String>,
) -> Result<Outcome, (&'static str, String)> {
    if domain == "script" {
        return crate::script::call_service(app, service, data, target, ctx, stack).await;
    }
    call_sync(app, domain, service, &data, &target, ctx).map(|_| Outcome::default())
}

/// The services that finish synchronously: the WebSocket calls these inline, so the state
/// changes they cause go out before their result.
pub fn call_sync(
    app: &App,
    domain: &str,
    service: &str,
    data: &Value,
    target: &Value,
    ctx: Context,
) -> Result<Context, (&'static str, String)> {
    let owner = |entity_id: &str, ctx: &Context| {
        app.mqtt
            .get()
            .is_some_and(|m| m.call(app, entity_id, service, data, ctx))
    };
    call(&app.states, &owner, domain, service, data, target, ctx)
}

/// An integration's handler for one entity: `true` if it owns (and handled) the entity.
pub type Owner<'a> = &'a dyn Fn(&str, &Context) -> bool;

/// Error as (websocket error code, message).
pub fn call(
    states: &States,
    owner: Owner,
    domain: &str,
    service: &str,
    data: &Value,
    target: &Value,
    ctx: Context,
) -> Result<Context, (&'static str, String)> {
    if (domain, service) == ("system_log", "write") {
        // The frontend reports its own errors here; HA logs them, so do we.
        eprintln!(
            "{}: {}",
            data["logger"].as_str().unwrap_or("frontend"),
            data["message"].as_str().unwrap_or_default()
        );
        return Ok(ctx);
    }
    if !(TOGGLEABLE.contains(&domain) || domain == "homeassistant") || !ACTIONS.contains(&service) {
        return Err((
            "not_found",
            format!("Service {domain}.{service} not found."),
        ));
    }
    let ids = entity_ids(&data["entity_id"])
        .into_iter()
        .chain(entity_ids(&target["entity_id"]));
    let all = states.all();
    for id in ids {
        // shortcut: only "all" and explicit ids; device/area/label targets need the registries (later step).
        let matches: Vec<_> = all
            .iter()
            .filter(|s| {
                if id == "all" {
                    s.entity_id.starts_with(&format!("{domain}."))
                } else {
                    *s.entity_id == *id
                }
            })
            .collect();
        for s in matches {
            let ent_domain = s.entity_id.split('.').next().unwrap_or_default();
            if !TOGGLEABLE.contains(&ent_domain)
                || (domain != "homeassistant" && domain != ent_domain)
            {
                continue;
            }
            if owner(&s.entity_id, &ctx) {
                continue;
            }
            let on = match service {
                "turn_on" => true,
                "turn_off" => false,
                _ => s.state != "on",
            };
            let new = if on { "on" } else { "off" };
            let attributes = if ent_domain == "light" {
                light_attrs(new)
            } else {
                s.attributes.clone()
            };
            states.set(&s.entity_id, new, attributes, ctx.clone());
        }
    }
    Ok(ctx)
}

fn entity_ids(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => s.split(',').map(|s| s.trim().to_string()).collect(),
        Value::Array(a) => a
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toggle_light() {
        let s = States::default();
        load_demo(&s);
        call(
            &s,
            &|_, _| false,
            "light",
            "toggle",
            &json!({"entity_id": "light.kitchen"}),
            &Value::Null,
            Context::new(Some("u".into())),
        )
        .unwrap();
        let st = s.get("light.kitchen").unwrap();
        assert_eq!(st.state, "on");
        assert_eq!(st.attributes["color_mode"], "onoff");
        // Domain mismatch: light.turn_off does not touch a switch.
        call(
            &s,
            &|_, _| false,
            "switch",
            "turn_on",
            &Value::Null,
            &json!({"entity_id": ["switch.fan"]}),
            Context::new(Some("u".into())),
        )
        .unwrap();
        call(
            &s,
            &|_, _| false,
            "light",
            "turn_off",
            &json!({"entity_id": "switch.fan"}),
            &Value::Null,
            Context::new(Some("u".into())),
        )
        .unwrap();
        assert_eq!(s.get("switch.fan").unwrap().state, "on");
        call(
            &s,
            &|_, _| false,
            "homeassistant",
            "turn_off",
            &json!({"entity_id": "switch.fan, light.kitchen"}),
            &Value::Null,
            Context::new(Some("u".into())),
        )
        .unwrap();
        assert_eq!(s.get("switch.fan").unwrap().state, "off");
        assert_eq!(s.get("light.kitchen").unwrap().state, "off");
        assert_eq!(
            call(
                &s,
                &|_, _| false,
                "light",
                "explode",
                &Value::Null,
                &Value::Null,
                Context::new(Some("u".into()))
            )
            .unwrap_err()
            .0,
            "not_found"
        );
    }
}
