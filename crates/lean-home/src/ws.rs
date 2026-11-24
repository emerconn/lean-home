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
use crate::state::Change;
use crate::{App, HA_VERSION, services};

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
    /// Subscriptions whose events we never fire yet; kept so `unsubscribe_events` succeeds.
    Silent,
}

struct Conn {
    app: Arc<App>,
    user: User,
    subs: HashMap<u64, Sub>,
    last_id: u64,
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
    let mut conn = Conn {
        app,
        user,
        subs: HashMap::new(),
        last_id: 0,
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
                let msg: Value = serde_json::from_str(&text).unwrap_or_default();
                for reply in conn.handle(msg) {
                    // A state change caused by this command (call_service) goes out before its result.
                    while let Ok(change) = rx.try_recv() {
                        conn.forward(&mut ws, &change).await?;
                    }
                    send(&mut ws, &reply).await?;
                }
            }
            change = rx.recv() => match change {
                Ok(change) => conn.forward(&mut ws, &change).await?,
                // Lagged: this client can't be brought back in sync cheaply; it resyncs on reconnect.
                Err(RecvError::Lagged(_) | RecvError::Closed) => return Ok(()),
            },
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

impl Conn {
    async fn forward(&self, ws: &mut WebSocket, change: &Change) -> Result<(), axum::Error> {
        // shortcut: serialized per subscriber; serialize once and fan out bytes when many clients matter.
        for (id, sub) in &self.subs {
            let ev = match sub {
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
            "get_services" => ok(services::describe()),
            "get_panels" => ok(panels()),
            "call_service" => {
                let (domain, service) = (
                    msg["domain"].as_str().unwrap_or_default(),
                    msg["service"].as_str().unwrap_or_default(),
                );
                match services::call(
                    &app.states,
                    domain,
                    service,
                    &msg["service_data"],
                    &msg["target"],
                    &self.user.id,
                ) {
                    Ok(ctx) => ok(json!({"context": ctx.json()})),
                    Err((code, m)) => vec![error(id, code, &m)],
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
            "render_template" => {
                let rendered = render_template(&msg);
                match rendered {
                    Ok(r) => {
                        self.subs.insert(id, Sub::Silent);
                        let listeners =
                            json!({"all": false, "entities": [], "domains": [], "time": false});
                        vec![
                            result(id, Value::Null),
                            event(id, json!({"result": r, "listeners": listeners})),
                        ]
                    }
                    Err(e) => vec![error(id, "template_error", &e)],
                }
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
            "config/entity_registry/list_for_display" => {
                ok(json!({"entity_categories": {"0": "config", "1": "diagnostic"}, "entities": []}))
            }
            "config/device_registry/list"
            | "config/area_registry/list"
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
            "usage_prediction/common_control" => ok(json!({"entities": []})),
            _ => vec![error(id, "unknown_command", "Unknown command.")],
        }
    }
}

/// Only the variables the frontend passes; HA's template globals (`states()`…) come with roadmap step 2.
fn render_template(msg: &Value) -> Result<String, String> {
    let mut env = minijinja::Environment::new();
    if msg["strict"] == true {
        env.set_undefined_behavior(minijinja::UndefinedBehavior::Strict);
    }
    let tpl = msg["template"].as_str().ok_or("template missing")?;
    env.render_str(tpl, minijinja::value::Serde(&msg["variables"]))
        .map_err(|e| e.to_string())
}

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
