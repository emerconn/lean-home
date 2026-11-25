//! End to end over real sockets: the frontend's login flow, token exchange, then the WebSocket.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::Message;

fn fixture_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lean-home-test-{}", crate::auth::random_hex(8)));
    let storage = dir.join("config/.storage");
    std::fs::create_dir_all(&storage).unwrap();
    std::fs::create_dir_all(dir.join("frontend")).unwrap();
    std::fs::write(
        dir.join("frontend/index.html"),
        "<meta name=theme-color content={{ theme_color }}>",
    )
    .unwrap();
    let hash = STANDARD.encode(bcrypt::hash("secret", 4).unwrap());
    let provider = json!({"version": 1, "minor_version": 1, "key": "auth_provider.homeassistant",
        "data": {"users": [{"username": "Dev", "password": hash}]}});
    let auth = json!({"version": 1, "minor_version": 1, "key": "auth", "data": {
        "users": [{"id": "u1", "group_ids": ["system-admin"], "is_owner": true, "is_active": true, "name": "Dev", "system_generated": false, "local_only": false}],
        "groups": [], "refresh_tokens": [],
        "credentials": [{"id": "c1", "user_id": "u1", "auth_provider_type": "homeassistant", "auth_provider_id": null, "data": {"username": "dev"}}],
    }});
    std::fs::write(
        storage.join("auth_provider.homeassistant"),
        provider.to_string(),
    )
    .unwrap();
    std::fs::write(storage.join("auth"), auth.to_string()).unwrap();
    dir
}

/// Minimal HTTP/1.1 client: returns (status, body).
async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    content_type: &str,
    body: &str,
) -> (u16, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).await.unwrap();
    let status = out[9..12].parse().unwrap();
    let body = out.split_once("\r\n\r\n").unwrap().1.to_string();
    (status, body)
}

#[tokio::test]
async fn login_then_websocket() {
    let dir = fixture_dir();
    let app = Arc::new(crate::App::load(&dir.join("config"), &dir.join("frontend"), true).unwrap());
    let router = crate::http::router(app, &dir.join("frontend"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .unwrap();
    });
    let base = format!("http://{addr}/");

    let (status, body) = http(addr, "GET", "/lovelace/0", "text/plain", "").await;
    assert_eq!(
        (status, body.as_str()),
        (200, "<meta name=theme-color content=#2980b9>")
    );

    // The frontend only uses PKCE if this advertises S256.
    let (status, body) = http(
        addr,
        "GET",
        "/.well-known/oauth-authorization-server",
        "text/plain",
        "",
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["code_challenge_methods_supported"],
        json!(["S256"])
    );
    assert_eq!(
        http(addr, "GET", "/.well-known/other", "text/plain", "")
            .await
            .0,
        404
    );
    assert_eq!(
        http(addr, "GET", "/api/nothing", "text/plain", "").await.0,
        404
    );

    // Login flow, as the frontend sends it: JSON bodies with a text/plain content type, PKCE.
    let verifier = "a".repeat(43);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(&verifier));
    let start = json!({"client_id": base, "handler": ["homeassistant", null], "redirect_uri": format!("{base}?auth_callback=1"),
        "code_challenge": challenge, "code_challenge_method": "S256", "response_type": "code"});
    let (status, body) = http(
        addr,
        "POST",
        "/auth/login_flow",
        "text/plain;charset=UTF-8",
        &start.to_string(),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let flow: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(flow["type"], "form");
    let flow_path = format!("/auth/login_flow/{}", flow["flow_id"].as_str().unwrap());

    let wrong = json!({"client_id": base, "username": "dev", "password": "nope"}).to_string();
    let (_, body) = http(addr, "POST", &flow_path, "text/plain", &wrong).await;
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["errors"]["base"],
        "invalid_auth"
    );

    // Usernames are case-insensitive, as in HA.
    let right = json!({"client_id": base, "username": "DEV ", "password": "secret"}).to_string();
    let (_, body) = http(addr, "POST", &flow_path, "text/plain", &right).await;
    let done: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(done["type"], "create_entry", "{body}");
    let code = done["result"].as_str().unwrap();

    // Token exchange as multipart, like the frontend; a wrong verifier is refused.
    let multipart = |fields: &[(&str, &str)]| {
        let mut b = String::new();
        for (k, v) in fields {
            b += &format!("--XX\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n");
        }
        b + "--XX--\r\n"
    };
    let ct = "multipart/form-data; boundary=XX";
    let mut fields = vec![
        ("grant_type", "authorization_code"),
        ("client_id", base.as_str()),
        ("code", code),
        ("code_verifier", "b"),
    ];
    let (status, _) = http(addr, "POST", "/auth/token", ct, &multipart(&fields)).await;
    assert_eq!(status, 400);
    fields[3].1 = &verifier;
    // The failed attempt consumed the code, as in HA (codes are single use).
    let (status, _) = http(addr, "POST", "/auth/token", ct, &multipart(&fields)).await;
    assert_eq!(status, 400);

    // Log in again and exchange properly.
    let (_, body) = http(
        addr,
        "POST",
        "/auth/login_flow",
        "text/plain",
        &start.to_string(),
    )
    .await;
    let flow: Value = serde_json::from_str(&body).unwrap();
    let flow_path = format!("/auth/login_flow/{}", flow["flow_id"].as_str().unwrap());
    let (_, body) = http(addr, "POST", &flow_path, "text/plain", &right).await;
    let code = serde_json::from_str::<Value>(&body).unwrap()["result"]
        .as_str()
        .unwrap()
        .to_string();
    fields[2].1 = &code;
    let (status, body) = http(addr, "POST", "/auth/token", ct, &multipart(&fields)).await;
    assert_eq!(status, 200, "{body}");
    let tokens: Value = serde_json::from_str(&body).unwrap();
    let access = tokens["access_token"].as_str().unwrap();

    // The refresh token was persisted in HA's format.
    let stored: Value =
        serde_json::from_str(&std::fs::read_to_string(dir.join("config/.storage/auth")).unwrap())
            .unwrap();
    assert_eq!(
        stored["data"]["refresh_tokens"][0]["token"],
        tokens["refresh_token"]
    );
    assert_eq!(stored["data"]["refresh_tokens"][0]["credential_id"], "c1");

    let refresh = format!(
        "grant_type=refresh_token&client_id={base}&refresh_token={}",
        tokens["refresh_token"].as_str().unwrap()
    );
    let (status, body) = http(
        addr,
        "POST",
        "/auth/token",
        "application/x-www-form-urlencoded",
        &refresh,
    )
    .await;
    assert_eq!(status, 200, "{body}");

    // WebSocket: a bad token is rejected.
    let url = format!("ws://{addr}/api/websocket");
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    let recv = async |ws: &mut tokio_tungstenite::WebSocketStream<_>| -> Value {
        let Some(Ok(Message::Text(t))) = ws.next().await else {
            panic!("socket closed")
        };
        serde_json::from_str(&t).unwrap()
    };
    assert_eq!(recv(&mut ws).await["type"], "auth_required");
    ws.send(Message::text(
        json!({"type": "auth", "access_token": "x.y.z"}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(recv(&mut ws).await["type"], "auth_invalid");

    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    recv(&mut ws).await;
    ws.send(Message::text(
        json!({"type": "auth", "access_token": access}).to_string(),
    ))
    .await
    .unwrap();
    assert_eq!(
        recv(&mut ws).await,
        json!({"type": "auth_ok", "ha_version": crate::HA_VERSION})
    );

    let cmd = |v: Value| Message::text(v.to_string());
    ws.send(cmd(json!({"id": 1, "type": "subscribe_entities"})))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await["result"], Value::Null);
    let snapshot = recv(&mut ws).await;
    assert_eq!(snapshot["event"]["a"]["light.kitchen"]["s"], "off");

    ws.send(cmd(
        json!({"id": 2, "type": "call_service", "domain": "light", "service": "toggle",
        "service_data": {"entity_id": "light.kitchen"}, "return_response": false}),
    ))
    .await
    .unwrap();
    // As in HA: the state change reaches subscribers before the call's result.
    let change = recv(&mut ws).await;
    assert_eq!(change["id"], 1);
    assert_eq!(change["event"]["c"]["light.kitchen"]["+"]["s"], "on");
    assert_eq!(
        change["event"]["c"]["light.kitchen"]["+"]["c"]["user_id"],
        "u1"
    );
    let res = recv(&mut ws).await;
    assert_eq!(
        (res["id"].clone(), res["success"].clone()),
        (json!(2), json!(true))
    );

    ws.send(cmd(json!({"id": 2, "type": "ping"})))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await["error"]["code"], "id_reuse");
    ws.send(cmd(json!({"id": 3, "type": "no/such_command"})))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await["error"]["code"], "unknown_command");

    // render_template: acknowledged, rendered, then re-rendered when what it read changes.
    ws.send(cmd(json!({"id": 4, "type": "render_template",
        "template": "{{ is_state('switch.fan', 'on') }} {{ states.light.kitchen.state }}"})))
        .await
        .unwrap();
    assert_eq!(recv(&mut ws).await["success"], true);
    assert_eq!(
        recv(&mut ws).await["event"],
        json!({"result": "False on", "listeners": {"all": false, "domains": [],
            "entities": ["switch.fan", "light.kitchen"], "time": false}})
    );
    ws.send(cmd(
        json!({"id": 5, "type": "call_service", "domain": "switch", "service": "toggle",
        "service_data": {"entity_id": "switch.fan"}}),
    ))
    .await
    .unwrap();
    // The entities diff (id 1) and the new render (id 4) come in either order, then the result.
    let mut by_id = std::collections::HashMap::new();
    for _ in 0..3 {
        let m = recv(&mut ws).await;
        by_id.insert(m["id"].as_u64().unwrap(), m);
    }
    assert_eq!(by_id[&4]["event"]["result"], "True on");
    assert_eq!(by_id[&5]["success"], true);
    // Results are parsed like HA's: a number stays a number.
    ws.send(cmd(
        json!({"id": 6, "type": "render_template", "template": "{{ 1 + 1 }}"}),
    ))
    .await
    .unwrap();
    recv(&mut ws).await;
    assert_eq!(recv(&mut ws).await["event"]["result"], 2);

    std::fs::remove_dir_all(dir).ok();
}
