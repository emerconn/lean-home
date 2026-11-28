//! `/api/websocket`: HA's WebSocket API. Command shapes follow `compat/fixtures/frontend-load/ws.jsonl`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::broadcast::error::{RecvError, TryRecvError};

use crate::auth::User;
use crate::state::{Change, Event};
use crate::{App, HA_VERSION, services, template};

pub async fn upgrade(State(app): State<Arc<App>>, ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(move |socket| async move {
        // Errors here mean the client went away; there is nobody left to report to.
        let _ = run(app, socket).await;
    })
}

enum Sub {
    Entities(Option<Vec<String>>),
    /// `None` = all event types
    Events(Option<String>),
    Template(Box<TemplateSub>),
    /// `subscribe_condition`: the condition, and the last event sent.
    Condition(Box<crate::condition::Condition>, Value),
    /// Subscriptions whose events we never fire yet; kept so `unsubscribe_events` succeeds.
    Silent,
}

/// A `render_template` subscription: re-rendered when what it read changes.
// shortcut: no rate limit (HA: 1/s for domain listeners, 1/min for all states); add if a
// dashboard iterating all states proves costly.
struct TemplateSub {
    template: String,
    variables: Value,
    strict: bool,
    report_errors: bool,
    listeners: template::Listeners,
    last: Option<Value>,
}

impl TemplateSub {
    /// The events a render sends, as HA's `handle_render_template` does: with `report_errors`,
    /// the lines the engine logged (`'x' is undefined`) and then any error; the result only when
    /// it changed.
    fn refresh(&mut self, states: &Arc<crate::state::States>) -> Vec<Value> {
        let (rendered, logs) =
            template::render_logged(states, &self.template, &self.variables, self.strict);
        let mut events: Vec<Value> = if self.report_errors {
            logs.into_iter()
                .map(|(level, msg)| json!({"error": msg, "level": level}))
                .collect()
        } else {
            vec![]
        };
        match rendered {
            Ok((text, listeners)) => {
                self.listeners = listeners;
                let r = template::result(&self.template, &text);
                if self.last.as_ref() != Some(&r) {
                    self.last = Some(r.clone());
                    events.push(json!({"result": r, "listeners": self.listeners.json()}));
                }
            }
            Err(e) => {
                // A failed render listens to everything, so it can recover (HA's RenderInfo).
                self.listeners = template::Listeners {
                    all: true,
                    ..Default::default()
                };
                self.last = None;
                if self.report_errors {
                    events.push(json!({"error": e, "level": "ERROR"}));
                }
            }
        }
        events
    }
}

/// A new `render_template` subscription and its first replies. As recorded from HA
/// (compat/fixtures/automations): setup renders once and logs (log and error lines, before the
/// result), then the first refresh sends its events again plus the result.
fn start_render_template(
    id: u64,
    tpl: &str,
    msg: &Value,
    states: &Arc<crate::state::States>,
) -> (TemplateSub, Vec<Value>) {
    let mut sub = TemplateSub {
        template: tpl.into(),
        variables: msg["variables"].clone(),
        strict: msg["strict"] == true,
        report_errors: msg["report_errors"] == true,
        listeners: Default::default(),
        last: None,
    };
    let first = sub.refresh(states);
    let replies = first
        .iter()
        .filter(|e| e.get("error").is_some())
        .map(|e| event(id, e.clone()))
        .chain(std::iter::once(result(id, Value::Null)))
        .chain(first.iter().map(|e| event(id, e.clone())))
        .collect();
    (sub, replies)
}

struct Conn {
    app: Arc<App>,
    user: User,
    subs: HashMap<u64, Sub>,
    last_id: u64,
    /// Results of commands that finish later (a `call_service` that runs a script).
    later: tokio::sync::mpsc::UnboundedSender<Value>,
}

async fn send(ws: &mut WebSocket, v: &Value) -> Result<(), axum::Error> {
    ws.send(Message::Text(v.to_string().into())).await
}

async fn run(app: Arc<App>, mut ws: WebSocket) -> Result<(), axum::Error> {
    send(
        &mut ws,
        &json!({"type": "auth_required", "ha_version": HA_VERSION}),
    )
    .await?;
    // HA gives a client 10 s to authenticate.
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.recv()).await;
    let Ok(Some(Ok(Message::Text(text)))) = msg else {
        return Ok(());
    };
    let auth: Value = serde_json::from_str(&text).unwrap_or_default();
    let user = (auth["type"] == "auth")
        .then(|| {
            auth["access_token"]
                .as_str()
                .and_then(|t| app.auth.validate(t))
        })
        .flatten();
    let Some(user) = user else {
        send(
            &mut ws,
            &json!({"type": "auth_invalid", "message": "Invalid access token or password"}),
        )
        .await?;
        return Ok(());
    };
    send(
        &mut ws,
        &json!({"type": "auth_ok", "ha_version": HA_VERSION}),
    )
    .await?;

    let (mut rx, _) = app.states.subscribe();
    let mut ev_rx = app.states.subscribe_events();
    let (later, mut later_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut conn = Conn {
        app,
        user,
        subs: HashMap::new(),
        last_id: 0,
        later,
    };
    loop {
        tokio::select! {
            msg = ws.recv() => {
                let text = match msg {
                    Some(Ok(Message::Text(t))) => t,
                    Some(Ok(Message::Close(_)) | Err(_)) | None => return Ok(()),
                    Some(Ok(_)) => continue,
                };
                // Forward changes that happened before this command first: keeps snapshots and
                // results ordered after them, as in HA.
                loop {
                    match rx.try_recv() {
                        Ok(change) => conn.forward(&mut ws, &change).await?,
                        Err(TryRecvError::Empty) => break,
                        Err(_) => return Ok(()),
                    }
                }
                loop {
                    match ev_rx.try_recv() {
                        Ok(ev) => conn.forward_event(&mut ws, &ev).await?,
                        Err(TryRecvError::Empty) => break,
                        Err(_) => return Ok(()),
                    }
                }
                let msg: Value = serde_json::from_str(&text).unwrap_or_default();
                for reply in conn.handle(msg) {
                    // A state change caused by this command (call_service) goes out before its result.
                    while let Ok(change) = rx.try_recv() {
                        conn.forward(&mut ws, &change).await?;
                    }
                    while let Ok(ev) = ev_rx.try_recv() {
                        conn.forward_event(&mut ws, &ev).await?;
                    }
                    send(&mut ws, &reply).await?;
                }
            }
            Some(reply) = later_rx.recv() => {
                // As for inline results: the state changes the call caused go out first.
                while let Ok(change) = rx.try_recv() {
                    conn.forward(&mut ws, &change).await?;
                }
                while let Ok(ev) = ev_rx.try_recv() {
                    conn.forward_event(&mut ws, &ev).await?;
                }
                send(&mut ws, &reply).await?;
            }
            change = rx.recv() => match change {
                Ok(change) => conn.forward(&mut ws, &change).await?,
                // Lagged: this client can't be brought back in sync cheaply; it resyncs on reconnect.
                Err(RecvError::Lagged(_) | RecvError::Closed) => return Ok(()),
            },
            ev = ev_rx.recv() => match ev {
                Ok(ev) => conn.forward_event(&mut ws, &ev).await?,
                Err(RecvError::Lagged(_) | RecvError::Closed) => return Ok(()),
            },
            // Templates that use now() re-render at the start of every minute, as in HA.
            _ = tokio::time::sleep(until_next_minute()), if conn.has_time() => conn.tick(&mut ws).await?,
        }
    }
}

fn result(id: u64, r: Value) -> Value {
    json!({"id": id, "type": "result", "success": true, "result": r})
}

fn error(id: u64, code: &str, message: &str) -> Value {
    json!({"id": id, "type": "result", "success": false, "error": {"code": code, "message": message}})
}

fn event(id: u64, e: Value) -> Value {
    json!({"id": id, "type": "event", "event": e})
}

// HA's SUBSCRIBE_ALLOWLIST: event types non-admin users may subscribe to.
const NON_ADMIN_EVENTS: &[&str] = &[
    "area_registry_updated",
    "component_loaded",
    "core_config_updated",
    "device_registry_updated",
    "entity_registry_updated",
    "repairs_issue_registry_updated",
    "lovelace_updated",
    "panels_updated",
    "recorder_5min_statistics_generated",
    "recorder_hourly_statistics_generated",
    "service_registered",
    "service_removed",
    "shopping_list_updated",
    "state_changed",
    "themes_updated",
    "label_registry_updated",
    "labs_updated",
    "category_registry_updated",
    "floor_registry_updated",
];

fn until_next_minute() -> Duration {
    let ms = chrono::Utc::now().timestamp_millis().rem_euclid(60_000) as u64;
    Duration::from_millis(60_000 - ms)
}

impl Conn {
    fn has_time(&self) -> bool {
        self.subs
            .values()
            .any(|s| matches!(s, Sub::Template(t) if t.listeners.time))
    }

    async fn tick(&mut self, ws: &mut WebSocket) -> Result<(), axum::Error> {
        for (id, sub) in &mut self.subs {
            if let Sub::Template(t) = sub
                && t.listeners.time
            {
                for ev in t.refresh(&self.app.states) {
                    send(ws, &event(*id, ev)).await?;
                }
            }
        }
        Ok(())
    }

    async fn forward(&mut self, ws: &mut WebSocket, change: &Change) -> Result<(), axum::Error> {
        // shortcut: serialized per subscriber; serialize once and fan out bytes when many clients matter.
        for (id, sub) in &mut self.subs {
            let ev = match sub {
                Sub::Template(t)
                    if t.listeners.matches(
                        &change.entity_id,
                        change.old.is_none() || change.new.is_none(),
                    ) =>
                {
                    for ev in t.refresh(&self.app.states) {
                        send(ws, &event(*id, ev)).await?;
                    }
                    continue;
                }
                // shortcut: re-checked on every state change, not only the entities it reads
                // (the event goes out only when the result changes, so clients see the same);
                // a `for:` isn't polled each second as HA does, it updates on the next change.
                Sub::Condition(c, last) => match condition_event(&self.app, c, last) {
                    Some(ev) => ev,
                    None => continue,
                },
                Sub::Entities(None) => change.diff(),
                Sub::Entities(Some(ids)) if ids.iter().any(|i| **i == *change.entity_id) => {
                    change.diff()
                }
                Sub::Events(None) => change.event(),
                Sub::Events(Some(t)) if t == "state_changed" => change.event(),
                _ => continue,
            };
            send(ws, &event(*id, ev)).await?;
        }
        Ok(())
    }

    async fn forward_event(&mut self, ws: &mut WebSocket, ev: &Event) -> Result<(), axum::Error> {
        for (id, sub) in &self.subs {
            match sub {
                Sub::Events(None) => send(ws, &event(*id, ev.json())).await?,
                Sub::Events(Some(t)) if t == &ev.event_type => {
                    send(ws, &event(*id, ev.json())).await?;
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn handle(&mut self, msg: Value) -> Vec<Value> {
        let Some(id) = msg["id"].as_u64() else {
            return vec![
                json!({"type": "result", "success": false, "error": {"code": "invalid_format", "message": "Message incorrectly formatted."}}),
            ];
        };
        if id <= self.last_id {
            return vec![error(id, "id_reuse", "Identifier values have to increase.")];
        }
        self.last_id = id;
        let app = &self.app;
        let ok = |r: Value| vec![result(id, r)];
        // Sync on purpose: on the current_thread runtime no state change can land between the
        // caller's drain of the bus and a snapshot taken here.
        match msg["type"].as_str().unwrap_or_default() {
            "ping" => vec![json!({"id": id, "type": "pong"})],
            "supported_features" => ok(Value::Null),
            "auth/current_user" => ok(json!({
                "id": self.user.id,
                "name": self.user.name,
                "is_owner": self.user.is_owner,
                "is_admin": self.user.is_admin,
                "credentials": [{"auth_provider_type": crate::auth::PROVIDER, "auth_provider_id": null}],
                "mfa_modules": [],
            })),
            "get_states" => ok(app.states.all().iter().map(|s| s.json()).collect()),
            "get_config" => ok(app.config_json()),
            "get_services" => ok(services::describe(app)),
            "get_panels" => ok(panels()),
            "call_service" => {
                let (domain, service) = (
                    msg["domain"].as_str().unwrap_or_default(),
                    msg["service"].as_str().unwrap_or_default(),
                );
                let ctx = crate::state::Context::new(Some(self.user.id.clone()));
                if domain == "script" || domain == "automation" {
                    // A script runs for a while (`script.blink` and `automation.trigger` wait for
                    // the run to finish): answer when it's done and keep serving this connection
                    // meanwhile, as HA does.
                    let (app, later) = (app.clone(), self.later.clone());
                    let (domain, service) = (domain.to_string(), service.to_string());
                    let (data, target) = (msg["service_data"].clone(), msg["target"].clone());
                    tokio::spawn(async move {
                        let r = services::dispatch(
                            &app,
                            &domain,
                            &service,
                            data,
                            target,
                            ctx.clone(),
                            vec![],
                        )
                        .await;
                        let _ = later.send(match r {
                            Ok(_) => result(id, json!({"context": ctx.json()})),
                            Err((code, m)) => error(id, code, &m),
                        });
                    });
                    return vec![];
                }
                match services::call_sync(
                    app,
                    domain,
                    service,
                    &msg["service_data"],
                    &msg["target"],
                    ctx,
                ) {
                    Ok(ctx) => ok(json!({"context": ctx.json()})),
                    Err((code, m)) => vec![error(id, code, &m)],
                }
            }
            "trace/list" => {
                let domain = msg["domain"].as_str().unwrap_or_default();
                ok(app.traces.list(domain, msg["item_id"].as_str()).into())
            }
            "trace/get" => {
                let (d, i, r) = (
                    msg["domain"].as_str(),
                    msg["item_id"].as_str(),
                    msg["run_id"].as_str(),
                );
                match (d, i, r) {
                    (Some(d), Some(i), Some(r)) => match app.traces.get(d, i, r) {
                        Some(t) => ok(t),
                        None => vec![error(id, "not_found", "The trace could not be found")],
                    },
                    _ => vec![error(id, "invalid_format", "required key not provided")],
                }
            }
            "subscribe_entities" => {
                let filter: Option<Vec<String>> = msg["entity_ids"].as_array().map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                });
                let added: serde_json::Map<_, _> = app
                    .states
                    .all()
                    .iter()
                    .filter(|s| {
                        filter
                            .as_ref()
                            .is_none_or(|f| f.iter().any(|i| **i == *s.entity_id))
                    })
                    .map(|s| (s.entity_id.to_string(), s.compressed()))
                    .collect();
                self.subs.insert(id, Sub::Entities(filter));
                vec![result(id, Value::Null), event(id, json!({"a": added}))]
            }
            "subscribe_events" => {
                let et = msg["event_type"]
                    .as_str()
                    .filter(|t| *t != "*")
                    .map(String::from);
                let allowed = self.user.is_admin
                    || et.as_deref().is_some_and(|t| NON_ADMIN_EVENTS.contains(&t));
                if !allowed {
                    return vec![error(id, "unauthorized", "Unauthorized")];
                }
                self.subs.insert(id, Sub::Events(et));
                ok(Value::Null)
            }
            "unsubscribe_events" => match msg["subscription"]
                .as_u64()
                .and_then(|s| self.subs.remove(&s))
            {
                Some(_) => ok(Value::Null),
                None => vec![error(id, "not_found", "Subscription not found.")],
            },
            "fire_event" => {
                if !self.user.is_admin {
                    return vec![error(id, "unauthorized", "Unauthorized")];
                }
                let Some(et) = msg["event_type"].as_str() else {
                    return vec![error(
                        id,
                        "invalid_format",
                        "required key not provided @ data['event_type']",
                    )];
                };
                let data = msg["event_data"].as_object().cloned().unwrap_or_default();
                let ctx = crate::state::Context::new(Some(self.user.id.clone()));
                app.states.fire_event(et, data, ctx.clone());
                ok(json!({"context": ctx.json()}))
            }
            "render_template" => {
                let Some(tpl) = msg["template"].as_str() else {
                    return vec![error(
                        id,
                        "invalid_format",
                        "required key not provided @ data['template']",
                    )];
                };
                let (sub, replies) = start_render_template(id, tpl, &msg, &app.states);
                self.subs.insert(id, Sub::Template(Box::new(sub)));
                replies
            }
            // Data stores we don't have yet: the frontend treats null as "not set".
            // shortcut: set_user_data/set_system_data stay unknown_command; persist to
            // .storage/frontend.* when the profile page needs it.
            "frontend/subscribe_user_data" | "frontend/subscribe_system_data" => {
                self.subs.insert(id, Sub::Silent);
                vec![event(id, json!({"value": null})), result(id, Value::Null)]
            }
            "frontend/get_user_data" | "frontend/get_system_data" => ok(json!({"value": null})),
            "frontend/get_themes" => {
                ok(json!({"themes": {}, "default_theme": "default", "default_dark_theme": null}))
            }
            // shortcut: no translations yet, so the UI shows raw states ("on"); bundle the native domains' en.json later.
            "frontend/get_translations" => ok(json!({"resources": {}})),
            "config/entity_registry/list_for_display" => ok(
                json!({"entity_categories": {"0": "config", "1": "diagnostic"}, "entities": app.registry.display_entities()}),
            ),
            "config/device_registry/list" => ok(app.registry.display_devices().into()),
            "config/area_registry/list"
            | "config/floor_registry/list"
            | "config/label_registry/list" => ok(json!([])),
            "repairs/list_issues" => ok(json!({"issues": []})),
            "lovelace/config" => vec![error(id, "config_not_found", "No config found.")],
            "lovelace/resources" => ok(json!([])),
            "lovelace/info" => ok(json!({"resource_mode": "storage"})),
            "persistent_notification/subscribe" => {
                self.subs.insert(id, Sub::Silent);
                vec![
                    result(id, Value::Null),
                    event(id, json!({"type": "current", "notifications": {}})),
                ]
            }
            "labs/subscribe" => {
                self.subs.insert(id, Sub::Silent);
                let feature = json!({
                    "preview_feature": msg["preview_feature"], "domain": msg["domain"], "enabled": false, "is_built_in": true,
                    "feedback_url": "", "learn_more_url": "", "report_issue_url": "",
                });
                vec![result(id, Value::Null), event(id, feature)]
            }
            "config_entries/flow/subscribe" => {
                self.subs.insert(id, Sub::Silent);
                vec![event(id, json!([])), result(id, Value::Null)]
            }
            "subscribe_condition" => {
                let cond = match crate::condition::Condition::parse(&msg["condition"]) {
                    Ok(c) => c,
                    Err(e) => return vec![error(id, "invalid_format", &e)],
                };
                let mut last = Value::Null;
                let ev = condition_event(app, &cond, &mut last);
                self.subs.insert(id, Sub::Condition(Box::new(cond), last));
                let mut out = vec![result(id, Value::Null)];
                out.extend(ev.map(|e| event(id, e)));
                out
            }
            "usage_prediction/common_control" => ok(json!({"entities": []})),
            // The automation and script editors.
            "config/entity_registry/list" => ok(app.registry.list_entities().into()),
            "config/category_registry/list" => ok(json!([])),
            "blueprint/list" => {
                let Some(domain) = msg["domain"].as_str() else {
                    return vec![error(
                        id,
                        "invalid_format",
                        "required key not provided @ data['domain']",
                    )];
                };
                ok(crate::blueprint::list(&app.config_dir, domain).into())
            }
            "blueprint/substitute" => {
                let (Some(domain), Some(path), Some(input)) = (
                    msg["domain"].as_str(),
                    msg["path"].as_str(),
                    msg["input"].as_object(),
                ) else {
                    return vec![error(id, "invalid_format", "missing domain, path or input")];
                };
                let raw = json!({"use_blueprint": {"path": path, "input": input}});
                match crate::blueprint::Blueprint::load(&app.config_dir, domain, path)
                    .and_then(|bp| bp.substitute(raw.as_object().unwrap()))
                {
                    Ok(config) => ok(json!({"substituted_config": config})),
                    Err(e) => vec![error(id, "unknown_error", &e)],
                }
            }
            // shortcut: no icons.json data yet; the frontend falls back to its built-in icons.
            "frontend/get_icons" => ok(json!({"resources": {}})),
            "manifest/list" => {
                let wanted: Vec<&str> = match msg["integrations"].as_array() {
                    Some(a) => a.iter().filter_map(Value::as_str).collect(),
                    None => app.components(),
                };
                ok(MANIFESTS
                    .iter()
                    .filter(|m| wanted.iter().any(|w| m["domain"] == *w))
                    .cloned()
                    .collect())
            }
            "config_entries/subscribe" => {
                self.subs.insert(id, Sub::Silent);
                let loaded: Vec<&str> = app.mqtt.get().map_or(vec![], |_| vec!["mqtt"]);
                vec![
                    result(id, Value::Null),
                    event(id, app.registry.config_entries_event(&loaded)),
                ]
            }
            // The new-style triggers and conditions with descriptions (HA lists e.g.
            // `light.turned_on`); the classic ones (`state`, `time`…) are built into the frontend.
            "trigger_platforms/subscribe" | "condition_platforms/subscribe" => {
                self.subs.insert(id, Sub::Silent);
                let mut platforms = json!({});
                if msg["type"] == "trigger_platforms/subscribe" && app.mqtt.get().is_some() {
                    platforms["mqtt"] = json!({"fields": {
                        "payload": {"example": "on", "required": false, "selector": {"text": {"multiline": false, "multiple": false}}},
                        "topic": {"example": "living_room/switch/ac", "required": true, "selector": {"text": {"multiline": false, "multiple": false}}},
                    }});
                }
                vec![result(id, Value::Null), event(id, platforms)]
            }
            _ => vec![error(id, "unknown_command", "Unknown command.")],
        }
    }
}

/// A `subscribe_condition` event when the result differs from `last` (HA's `evaluate_condition`).
// shortcut: template errors aren't forwarded as `template_errors` yet.
fn condition_event(app: &App, c: &crate::condition::Condition, last: &mut Value) -> Option<Value> {
    let vars = serde_json::Map::new();
    let ev = crate::condition::Eval {
        states: &app.states,
        vars: &vars,
        trace: None,
    };
    let now = match c.check(&ev, "", None) {
        Ok(b) => json!({"result": b}),
        Err(e) => json!({"error": e}),
    };
    (now != *last).then(|| {
        *last = now.clone();
        now
    })
}

/// The manifests of the integrations lean-home implements, as HA 2026.11.2 sends them (taken
/// from the `automations` recording; regenerate with the HA upgrade, see docs/compatibility.md).
static MANIFESTS: std::sync::LazyLock<Vec<Value>> = std::sync::LazyLock::new(|| {
    serde_json::from_str(include_str!("manifests.json")).expect("manifests.json")
});

/// Panels that need no backend of their own: dashboards the frontend generates from entities.
/// `home` is the default panel since 2026.x; without it the frontend opens "Page not found".
fn panels() -> Value {
    let panel = |name: &str,
                 component: &str,
                 icon: Value,
                 title: Value,
                 admin: bool,
                 sidebar: bool| {
        json!({
            "component_name": component, "icon": icon, "title": title, "default_visible": true, "config": null,
            "url_path": name, "require_admin": admin, "config_panel_domain": null, "show_in_sidebar": sidebar,
        })
    };
    let strategy =
        |name: &str, icon: &str| panel(name, name, json!(icon), json!(name), false, false);
    json!({
        "config": panel("config", "config", json!("mdi:cog"), json!("config"), true, true),
        "lovelace": panel("lovelace", "lovelace", Value::Null, Value::Null, false, true),
        "light": strategy("light", "mdi:lamps"),
        "security": strategy("security", "mdi:security"),
        "climate": strategy("climate", "mdi:home-thermometer"),
        "home": strategy("home", "mdi:home"),
        "maintenance": strategy("maintenance", "mdi:wrench"),
        "profile": panel("profile", "profile", Value::Null, Value::Null, false, true),
        "notfound": panel("notfound", "notfound", Value::Null, Value::Null, false, true),
        "_my_redirect": panel("_my_redirect", "my", Value::Null, Value::Null, false, true),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every `render_template` the frontend and the scenario sent in the automations recording
    /// whose replies don't depend on device states: lean-home must answer exactly as HA did.
    #[test]
    fn render_template_replies_match_ha() {
        let msgs: Vec<(Value, Value)> =
            include_str!("../../../compat/fixtures/automations/ws.jsonl")
                .lines()
                .map(|l| serde_json::from_str::<Value>(l).unwrap())
                .flat_map(|l| {
                    let dir = l["dir"].clone();
                    match &l["msg"] {
                        Value::Array(a) => a
                            .iter()
                            .map(|m| (dir.clone(), m.clone()))
                            .collect::<Vec<_>>(),
                        m => vec![(dir, m.clone())],
                    }
                })
                .collect();
        let states = Arc::new(crate::state::States::default());
        let mut checked = 0;
        for (_, sent) in msgs
            .iter()
            .filter(|(d, m)| d == "send" && m["type"] == "render_template")
        {
            let tpl = sent["template"].as_str().unwrap();
            // The live device template is checked against its whole sequence in template.rs.
            if tpl.contains("demo_device") {
                continue;
            }
            let id = sent["id"].as_u64().unwrap();
            let want: Vec<&Value> = msgs
                .iter()
                .filter(|(d, m)| d == "recv" && m["id"] == id)
                .map(|(_, m)| m)
                .collect();
            let (_, got) = start_render_template(id, tpl, sent, &states);
            assert_eq!(
                got.iter().collect::<Vec<_>>(),
                want,
                "render_template {tpl:?}"
            );
            checked += 1;
        }
        // "## Welcome {{ user }}", four failing templates, one with an undefined variable
        assert_eq!(checked, 6);
    }
}
