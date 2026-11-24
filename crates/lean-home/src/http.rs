//! HTTP routes: the frontend's static files and index page, plus the `/auth/*` endpoints.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, FromRequest, Multipart, Path as UrlPath, Request, State};
use axum::http::{StatusCode, header};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::Deserialize;
use serde_json::{Value, json};
use tower_http::services::{ServeDir, ServeFile};

use crate::App;
use crate::auth::{self, PROVIDER, Step};

pub fn router(app: Arc<App>, frontend: &Path) -> Router {
    let mut r = Router::new()
        .route("/api/websocket", get(crate::ws::upgrade))
        .route("/auth/providers", get(providers))
        .route(
            "/auth/login_flow",
            post(flow_start).get(|| async { StatusCode::METHOD_NOT_ALLOWED }),
        )
        .route(
            "/auth/login_flow/{flow_id}",
            post(flow_step).delete(flow_abort).get(flow_get),
        )
        .route("/auth/token", post(token))
        .route("/auth/revoke", post(revoke))
        .route("/manifest.json", get(manifest))
        .route(
            "/.well-known/oauth-authorization-server",
            get(oauth_metadata),
        )
        .route_service(
            "/auth/authorize",
            ServeFile::new(frontend.join("authorize.html")),
        )
        .fallback(get(index));
    for f in [
        "service_worker.js",
        "sw-modern.js",
        "sw-modern.js.map",
        "sw-legacy.js",
        "sw-legacy.js.map",
        "robots.txt",
    ] {
        r = r.route_service(&format!("/{f}"), ServeFile::new(frontend.join(f)));
    }
    for d in ["static", "frontend_latest", "frontend_es5"] {
        let dir = ServeDir::new(frontend.join(d))
            .precompressed_br()
            .precompressed_gzip();
        r = r.nest_service(&format!("/{d}"), dir);
    }
    r.with_state(app)
}

/// HA renders `index.html` as a Jinja template (frontend IndexView).
pub fn render_index(frontend: &Path) -> Result<String, String> {
    let tpl = std::fs::read_to_string(frontend.join("index.html"))
        .map_err(|e| format!("frontend index.html: {e}"))?;
    let ctx = minijinja::context! {
        theme_color => THEME_COLOR,
        extra_modules => Vec::<String>::new(),
        extra_js_es5 => Vec::<String>::new(),
    };
    minijinja::Environment::new()
        .render_str(&tpl, ctx)
        .map_err(|e| e.to_string())
}

const THEME_COLOR: &str = "#2980b9";

// shortcut: unmatched GETs serve the app shell; HA 404s paths that aren't a registered panel.
// Upgrade when panels are registered dynamically.
async fn index(State(app): State<Arc<App>>, uri: axum::http::Uri) -> Response {
    // Never for API-ish paths: clients probe these and must see a 404, not HTML.
    if ["/.", "/api/", "/auth/"]
        .iter()
        .any(|p| uri.path().starts_with(p))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    Html(app.index_html.clone()).into_response()
}

/// RFC 8414 metadata. The frontend only uses PKCE when this advertises S256.
async fn oauth_metadata() -> Response {
    // shortcut: relative URLs and no `issuer` (HA's fallback when it can't tell its own URL);
    // add both once external_url/internal_url are read from core.config.
    let body = json!({
        "authorization_endpoint": "/auth/authorize",
        "token_endpoint": "/auth/token",
        "revocation_endpoint": "/auth/revoke",
        "grant_types_supported": ["authorization_code", "refresh_token"],
        "token_endpoint_auth_methods_supported": ["none"],
        "revocation_endpoint_auth_methods_supported": ["none"],
        // HA says true; we don't fetch client_id documents (see auth::redirect_ok).
        "client_id_metadata_document_supported": false,
        "code_challenge_methods_supported": ["S256"],
        "response_types_supported": ["code"],
        "service_documentation": "https://contributors.home-assistant.io/core/internals/auth-api/",
    });
    ([(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")], Json(body)).into_response()
}

async fn manifest() -> Response {
    let sizes = |s: &[u32], name: &str, purpose: &str| -> Vec<Value> {
        s.iter()
            .map(|n| json!({"src": format!("/static/icons/{name}-{n}x{n}.png"), "sizes": format!("{n}x{n}"), "type": "image/png", "purpose": purpose}))
            .collect()
    };
    let mut icons = sizes(&[192, 384, 512, 1024], "favicon", "any");
    icons.extend(sizes(
        &[48, 72, 96, 128, 192, 384, 512],
        "maskable_icon",
        "maskable",
    ));
    let body = json!({
        "background_color": "#FFFFFF",
        "description": "Home automation platform that puts local control and privacy first.",
        "dir": "ltr",
        "display": "standalone",
        "icons": icons,
        "screenshots": [{"src": "/static/images/screenshots/screenshot-1.png", "sizes": "413x792", "type": "image/png"}],
        "lang": "en-US",
        "name": "Home Assistant",
        "short_name": "Home Assistant",
        "start_url": "/?homescreen=1",
        "id": "/?homescreen=1",
        "theme_color": THEME_COLOR,
        "prefer_related_applications": true,
        "related_applications": [{"platform": "play", "id": "io.homeassistant.companion.android"}],
    });
    (
        [
            (header::CONTENT_TYPE, "application/manifest+json"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
        ],
        body.to_string(),
    )
        .into_response()
}

fn message(status: StatusCode, msg: &str) -> Response {
    (status, Json(json!({"message": msg}))).into_response()
}

async fn providers(ConnectInfo(addr): ConnectInfo<SocketAddr>) -> Json<Value> {
    let ip = addr.ip().to_canonical();
    let local = ip.is_loopback()
        || match ip {
            std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
            std::net::IpAddr::V6(v6) => v6.is_unique_local() || v6.is_unicast_link_local(),
        };
    Json(json!({
        "providers": [{"name": "Home Assistant Local", "id": null, "type": PROVIDER}],
        "preselect_remember_me": local,
    }))
}

#[derive(Deserialize)]
struct FlowStart {
    client_id: String,
    handler: (String, Option<String>),
    redirect_uri: String,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    response_type: Option<String>,
}

fn form(flow_id: &str, errors: Value) -> Value {
    json!({
        "type": "form",
        "flow_id": flow_id,
        "handler": [PROVIDER, null],
        "data_schema": [
            {"type": "string", "name": "username", "required": true},
            {"type": "string", "name": "password", "required": true},
        ],
        "errors": errors,
        "description_placeholders": null,
        "last_step": null,
        "preview": null,
        "step_id": "init",
    })
}

async fn flow_start(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    body: Bytes,
) -> Response {
    // JSON whatever the content type says (the frontend sends text/plain), as in HA.
    let Ok(req) = serde_json::from_slice::<FlowStart>(&body) else {
        return message(StatusCode::BAD_REQUEST, "Message format incorrect");
    };
    if !auth::client_id_ok(&req.client_id) {
        return message(StatusCode::BAD_REQUEST, "Invalid client id");
    }
    if req.response_type.as_deref().is_some_and(|t| t != "code") {
        return message(StatusCode::BAD_REQUEST, "Response type not supported");
    }
    if req.code_challenge_method.is_some() && req.code_challenge.is_none() {
        return message(
            StatusCode::BAD_REQUEST,
            "code_challenge required when code_challenge_method is provided",
        );
    }
    if let Some(ch) = &req.code_challenge {
        if req.code_challenge_method.as_deref() != Some("S256") {
            return message(StatusCode::BAD_REQUEST, "Transform algorithm not supported");
        }
        // S256 challenges are always 43 unpadded base64url characters.
        if ch.len() != 43
            || !ch
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return message(StatusCode::BAD_REQUEST, "Message format incorrect");
        }
    }
    if req.handler != (PROVIDER.to_string(), None) {
        return message(StatusCode::NOT_FOUND, "Invalid handler specified");
    }
    let id = app.auth.new_flow(
        addr.ip().to_canonical(),
        req.redirect_uri,
        req.code_challenge,
    );
    Json(form(&id, json!({}))).into_response()
}

async fn flow_step(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    UrlPath(flow_id): UrlPath<String>,
    body: Bytes,
) -> Response {
    let Ok(mut req) = serde_json::from_slice::<HashMap<String, Value>>(&body) else {
        return message(StatusCode::BAD_REQUEST, "Message format incorrect");
    };
    let Some(client_id) = req
        .remove("client_id")
        .and_then(|v| v.as_str().map(String::from))
    else {
        return message(StatusCode::BAD_REQUEST, "Message format incorrect");
    };
    if !auth::client_id_ok(&client_id) {
        return message(StatusCode::BAD_REQUEST, "Invalid client id");
    }
    if !app.auth.has_flow(&flow_id) {
        return message(StatusCode::NOT_FOUND, "Invalid flow specified");
    }
    let field = |k: &str| req.get(k).and_then(Value::as_str).map(String::from);
    let (Some(username), Some(password)) = (field("username"), field("password")) else {
        return message(StatusCode::BAD_REQUEST, "User input malformed");
    };
    let ip = addr.ip().to_canonical();
    let guard = &app.auth.guard;
    tokio::time::sleep(guard.delay(ip)).await;
    let ok = guard.verify(&password, app.auth.hash_for(&username)).await;
    let step = app
        .auth
        .finish_flow(&flow_id, ip, &client_id, &username, ok);
    let failed = matches!(step, Step::InvalidAuth);
    if failed || matches!(step, Step::Done(_)) {
        guard.record(ip, !failed);
    }
    if failed {
        // HA logs this too (http/ban.py); usernames are left out of the log on purpose.
        eprintln!("Login attempt with invalid authentication from {ip}");
    }
    match step {
        Step::NotFound => message(StatusCode::NOT_FOUND, "Invalid flow specified"),
        Step::IpChanged => message(StatusCode::BAD_REQUEST, "IP address changed"),
        Step::BadRedirect => message(StatusCode::FORBIDDEN, "Invalid redirect URI"),
        Step::InvalidAuth => Json(form(&flow_id, json!({"base": "invalid_auth"}))).into_response(),
        Step::Done(code) => Json(json!({
            "type": "create_entry",
            "flow_id": flow_id,
            "handler": [PROVIDER, null],
            "description": null,
            "description_placeholders": null,
            "result": code,
        }))
        .into_response(),
    }
}

async fn flow_get() -> Response {
    message(StatusCode::NOT_FOUND, "Invalid flow specified")
}

async fn flow_abort(State(app): State<Arc<App>>, UrlPath(flow_id): UrlPath<String>) -> Response {
    if app.auth.abort_flow(&flow_id) {
        message(StatusCode::OK, "Flow aborted")
    } else {
        message(StatusCode::NOT_FOUND, "Invalid flow specified")
    }
}

/// The frontend posts `/auth/token` as multipart/form-data, other clients urlencoded; HA accepts both.
async fn form_pairs(req: Request) -> Option<Vec<(String, String)>> {
    let multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("multipart/form-data"));
    if !multipart {
        return Form::<Vec<(String, String)>>::from_request(req, &())
            .await
            .ok()
            .map(|Form(p)| p);
    }
    let mut mp = Multipart::from_request(req, &()).await.ok()?;
    let mut pairs = vec![];
    while let Some(field) = mp.next_field().await.ok()? {
        let name = field.name().unwrap_or_default().to_string();
        pairs.push((name, field.text().await.ok()?));
    }
    Some(pairs)
}

async fn token(
    State(app): State<Arc<App>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: Request,
) -> Response {
    let Some(pairs) = form_pairs(req).await else {
        return token_reply(Err((400, json!({"error": "invalid_request"}))));
    };
    let data: HashMap<_, _> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    // RFC 6749 3.2: parameters must not be included more than once.
    if data.len() != pairs.len() {
        return token_reply(Err((400, json!({"error": "invalid_request"}))));
    }
    if data.get("action") == Some(&"revoke") {
        app.auth
            .revoke(data.get("token").copied().unwrap_or_default());
        return StatusCode::OK.into_response();
    }
    let ip = addr.ip().to_canonical();
    let client_id = data.get("client_id").copied();
    if client_id.is_some_and(|c| !auth::client_id_ok(c)) {
        return token_reply(Err((
            400,
            json!({"error": "invalid_request", "error_description": "Invalid client id"}),
        )));
    }
    token_reply(match data.get("grant_type").copied() {
        Some("authorization_code") => match (client_id, data.get("code")) {
            (None, _) => Err((
                400,
                json!({"error": "invalid_request", "error_description": "Invalid client id"}),
            )),
            (_, None) => Err((
                400,
                json!({"error": "invalid_request", "error_description": "Invalid code"}),
            )),
            (Some(c), Some(code)) => {
                app.auth
                    .exchange_code(c, code, data.get("code_verifier").copied(), ip)
            }
        },
        Some("refresh_token") => match data.get("refresh_token") {
            None => Err((400, json!({"error": "invalid_request"}))),
            Some(t) => app.auth.refresh(client_id, t, ip),
        },
        _ => Err((400, json!({"error": "unsupported_grant_type"}))),
    })
}

fn token_reply(r: Result<Value, auth::TokenError>) -> Response {
    let (status, body) = r.map_or_else(|e| e, |ok| (200, ok));
    let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST);
    (
        status,
        [
            (header::CACHE_CONTROL, "no-store"),
            (header::PRAGMA, "no-cache"),
        ],
        Json(body),
    )
        .into_response()
}

async fn revoke(
    State(app): State<Arc<App>>,
    Form(data): Form<HashMap<String, String>>,
) -> StatusCode {
    if let Some(t) = data.get("token") {
        app.auth.revoke(t);
    }
    StatusCode::OK
}
