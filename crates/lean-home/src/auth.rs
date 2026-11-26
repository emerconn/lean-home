//! HA-compatible auth. Reads `.storage/auth_provider.homeassistant` (bcrypt hashes) and reads/writes
//! `.storage/auth` (users, credentials, refresh tokens) in HA's own format, so an existing HA config
//! dir logs in unchanged and tokens issued by either side are valid on the other.

use std::collections::HashMap;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use hmac::{Hmac, KeyInit, Mac};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const PROVIDER: &str = "homeassistant";
pub const ACCESS_TOKEN_SECS: i64 = 1800;
const REFRESH_TOKEN_SECS: f64 = 90.0 * 86400.0;
const CODE_TTL: Duration = Duration::from_secs(600);
// Any valid bcrypt hash: checked against for unknown users so timing doesn't reveal which exist.
const DUMMY_HASH: &str = "$2b$12$CiuFGszHx9eNHxPuQcwBWez4CwDTOcLTX5CbOpV6gef2nYuXkY7BO";

/// Throttles password checks. Stock HA has no limit by default (`login_attempts_threshold: -1`)
/// and relies on bcrypt's cost; on one small CPU that cost is itself the risk, so checks run
/// one at a time, and an IP that keeps failing waits longer before each check. Responses are
/// unchanged, only slower.
// shortcut: no IP ban (HA's opt-in `http: login_attempts_threshold`); add with configuration.yaml.
pub struct LoginGuard {
    /// One bcrypt check at a time: parallel attempts queue instead of each taking a core.
    check: tokio::sync::Semaphore,
    /// ip -> (failures in the current window, window start)
    failures: Mutex<HashMap<IpAddr, (u32, Instant)>>,
}

const FAILURE_WINDOW: Duration = Duration::from_secs(15 * 60);
const FREE_FAILURES: u32 = 5;
const MAX_DELAY: Duration = Duration::from_secs(30);
/// Bounds memory if an attacker rotates addresses; backoff then restarts, the queue still holds.
const MAX_TRACKED: usize = 4096;

impl Default for LoginGuard {
    fn default() -> Self {
        Self {
            check: tokio::sync::Semaphore::new(1),
            failures: Mutex::default(),
        }
    }
}

impl LoginGuard {
    /// How long `ip` waits before its next check: nothing for the first few failures, then
    /// 1 s doubling up to 30 s.
    pub fn delay(&self, ip: IpAddr) -> Duration {
        let failures = self.failures.lock().unwrap();
        match failures.get(&ip) {
            Some((n, start)) if *n >= FREE_FAILURES && start.elapsed() < FAILURE_WINDOW => {
                Duration::from_secs(1 << (n - FREE_FAILURES).min(5)).min(MAX_DELAY)
            }
            _ => Duration::ZERO,
        }
    }

    pub fn record(&self, ip: IpAddr, ok: bool) {
        let mut failures = self.failures.lock().unwrap();
        if ok {
            failures.remove(&ip);
            return;
        }
        if failures.len() >= MAX_TRACKED {
            failures.retain(|_, (_, start)| start.elapsed() < FAILURE_WINDOW);
            if failures.len() >= MAX_TRACKED {
                failures.clear();
            }
        }
        let e = failures.entry(ip).or_insert((0, Instant::now()));
        if e.1.elapsed() >= FAILURE_WINDOW {
            *e = (0, Instant::now());
        }
        e.0 += 1;
    }

    /// Check `password` against a bcrypt `hash`, one check at a time, off the async thread.
    pub async fn verify(&self, password: &str, hash: String) -> bool {
        let Ok(_permit) = self.check.acquire().await else {
            return false;
        };
        // bcrypt truncates at 72 bytes, as HA does explicitly.
        let pw = password.as_bytes()[..password.len().min(72)].to_vec();
        tokio::task::spawn_blocking(move || bcrypt::verify(pw, &hash).unwrap_or(false))
            .await
            .unwrap_or(false)
    }
}

pub struct Auth {
    pub guard: LoginGuard,
    path: PathBuf,
    store: Mutex<Value>,
    /// normalized username -> bcrypt hash
    hashes: HashMap<String, String>,
    flows: Mutex<HashMap<String, Flow>>,
    codes: Mutex<HashMap<String, Code>>,
}

struct Flow {
    ip: IpAddr,
    redirect_uri: String,
    challenge: Option<String>,
}

struct Code {
    client_id: String,
    credential_id: String,
    challenge: Option<String>,
    created: Instant,
}

#[derive(Clone, Debug)]
pub struct User {
    pub id: String,
    pub name: String,
    pub is_owner: bool,
    pub is_admin: bool,
}

pub enum Step {
    NotFound,
    IpChanged,
    InvalidAuth,
    BadRedirect,
    Done(String),
}

/// (HTTP status, OAuth error body)
pub type TokenError = (u16, Value);

impl Auth {
    pub fn load(config: &Path) -> Result<Self, String> {
        let storage = config.join(".storage");
        let read = |name: &str| -> Result<Value, String> {
            let p = storage.join(name);
            let s = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))
        };
        let provider = read("auth_provider.homeassistant")?;
        let hashes: HashMap<_, _> = provider["data"]["users"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|u| {
                let hash = STANDARD.decode(u["password"].as_str()?).ok()?;
                Some((
                    normalize(u["username"].as_str()?),
                    String::from_utf8(hash).ok()?,
                ))
            })
            .collect();
        // shortcut: no onboarding yet, so a config dir onboarded by HA is required; upgrade with roadmap step 5.
        if hashes.is_empty() {
            return Err(
                "no users in .storage/auth_provider.homeassistant; onboard with HA first".into(),
            );
        }
        Ok(Self {
            guard: LoginGuard::default(),
            path: storage.join("auth"),
            store: Mutex::new(read("auth")?),
            hashes,
            flows: Mutex::default(),
            codes: Mutex::default(),
        })
    }

    pub fn new_flow(&self, ip: IpAddr, redirect_uri: String, challenge: Option<String>) -> String {
        let id = random_hex(16);
        let flow = Flow {
            ip,
            redirect_uri,
            challenge,
        };
        self.flows.lock().unwrap().insert(id.clone(), flow);
        id
    }

    pub fn has_flow(&self, flow_id: &str) -> bool {
        self.flows.lock().unwrap().contains_key(flow_id)
    }

    pub fn abort_flow(&self, flow_id: &str) -> bool {
        self.flows.lock().unwrap().remove(flow_id).is_some()
    }

    /// Bcrypt hash to check `username` against; a dummy one for unknown users.
    pub fn hash_for(&self, username: &str) -> String {
        self.hashes
            .get(&normalize(username))
            .map_or(DUMMY_HASH, |h| h)
            .to_string()
    }

    /// Finish the login step once the password check (run off the async thread) is done.
    pub fn finish_flow(
        &self,
        flow_id: &str,
        ip: IpAddr,
        client_id: &str,
        username: &str,
        password_ok: bool,
    ) -> Step {
        let mut flows = self.flows.lock().unwrap();
        let Some(flow) = flows.get(flow_id) else {
            return Step::NotFound;
        };
        if flow.ip != ip {
            return Step::IpChanged;
        }
        if !password_ok || !self.hashes.contains_key(&normalize(username)) {
            return Step::InvalidAuth;
        }
        if !redirect_ok(client_id, &flow.redirect_uri) {
            return Step::BadRedirect;
        }
        // shortcut: only credentials HA already linked to a user; creating users comes with onboarding.
        let Some(credential_id) = self.credential_for(username) else {
            return Step::InvalidAuth;
        };
        let flow = flows.remove(flow_id).unwrap();
        let code = random_hex(16);
        let mut codes = self.codes.lock().unwrap();
        codes.retain(|_, c| c.created.elapsed() < CODE_TTL);
        codes.insert(
            code.clone(),
            Code {
                client_id: client_id.into(),
                credential_id,
                challenge: flow.challenge,
                created: Instant::now(),
            },
        );
        Step::Done(code)
    }

    fn credential_for(&self, username: &str) -> Option<String> {
        let store = self.store.lock().unwrap();
        let username = normalize(username);
        store["data"]["credentials"]
            .as_array()?
            .iter()
            .find_map(|c| {
                (c["auth_provider_type"] == PROVIDER
                    && c["data"]["username"].as_str().map(normalize) == Some(username.clone()))
                .then(|| c["id"].as_str().map(String::from))?
            })
    }

    /// `grant_type=authorization_code`
    pub fn exchange_code(
        &self,
        client_id: &str,
        code: &str,
        verifier: Option<&str>,
        ip: IpAddr,
    ) -> Result<Value, TokenError> {
        let bad = |desc: &str| {
            (
                400,
                json!({"error": "invalid_request", "error_description": desc}),
            )
        };
        let entry = self.codes.lock().unwrap().remove(code);
        let Some(entry) =
            entry.filter(|c| c.client_id == client_id && c.created.elapsed() < CODE_TTL)
        else {
            return Err(bad("Invalid code"));
        };
        match (&entry.challenge, verifier) {
            (Some(_), None | Some("")) => return Err(bad("Code verifier required")),
            (Some(ch), Some(v)) if URL_SAFE_NO_PAD.encode(Sha256::digest(v)) != *ch => {
                return Err((
                    400,
                    json!({"error": "invalid_grant", "error_description": "Invalid code verifier"}),
                ));
            }
            (None, Some(_)) => {
                return Err(bad(
                    "Code verifier provided but no code challenge was present",
                ));
            }
            _ => {}
        }
        let mut store = self.store.lock().unwrap();
        let data = &mut store["data"];
        let user_id = data["credentials"]
            .as_array()
            .and_then(|cs| cs.iter().find(|c| c["id"] == entry.credential_id.as_str()))
            .and_then(|c| c["user_id"].as_str())
            .map(String::from)
            .ok_or_else(|| bad("Invalid code"))?;
        if !find(data, "users", &user_id).is_some_and(|u| u["is_active"] == true) {
            return Err((
                403,
                json!({"error": "access_denied", "error_description": "User is not active"}),
            ));
        }
        let now = now_secs();
        let token = json!({
            "id": random_hex(16),
            "user_id": user_id,
            "client_id": client_id,
            "client_name": null,
            "client_icon": null,
            "token_type": "normal",
            "created_at": iso_now(),
            "access_token_expiration": ACCESS_TOKEN_SECS as f64,
            "token": random_hex(64),
            "jwt_key": random_hex(64),
            "last_used_at": iso_now(),
            "last_used_ip": ip.to_string(),
            "expire_at": now + REFRESH_TOKEN_SECS,
            "credential_id": entry.credential_id,
            "version": crate::HA_VERSION,
        });
        let access = access_token(&token);
        let refresh = token["token"].clone();
        data["refresh_tokens"]
            .as_array_mut()
            .ok_or_else(|| bad("Invalid code"))?
            .push(token);
        self.save(&store);
        Ok(json!({
            "access_token": access,
            "token_type": "Bearer",
            "refresh_token": refresh,
            "expires_in": ACCESS_TOKEN_SECS,
            "ha_auth_provider": PROVIDER,
        }))
    }

    /// `grant_type=refresh_token`
    pub fn refresh(
        &self,
        client_id: Option<&str>,
        token: &str,
        ip: IpAddr,
    ) -> Result<Value, TokenError> {
        let mut store = self.store.lock().unwrap();
        let now = now_secs();
        let tokens = store["data"]["refresh_tokens"]
            .as_array_mut()
            .ok_or((400, json!({"error": "invalid_grant"})))?;
        let Some(rt) = tokens
            .iter_mut()
            .find(|t| t["token"] == token && t["expire_at"].as_f64().is_none_or(|e| e > now))
        else {
            return Err((400, json!({"error": "invalid_grant"})));
        };
        if rt["client_id"].as_str() != client_id {
            return Err((400, json!({"error": "invalid_request"})));
        }
        // Same bookkeeping as HA's async_log_refresh_token_usage.
        rt["last_used_at"] = iso_now().into();
        rt["last_used_ip"] = ip.to_string().into();
        if rt["expire_at"].is_number() {
            rt["expire_at"] = (now + REFRESH_TOKEN_SECS).into();
        }
        let access = access_token(rt);
        self.save(&store);
        Ok(json!({"access_token": access, "token_type": "Bearer", "expires_in": ACCESS_TOKEN_SECS}))
    }

    /// `/auth/revoke`: deletes the refresh token; HA answers 200 whether or not it existed.
    pub fn revoke(&self, token: &str) {
        let mut store = self.store.lock().unwrap();
        let Some(tokens) = store["data"]["refresh_tokens"].as_array_mut() else {
            return;
        };
        let before = tokens.len();
        tokens.retain(|t| t["token"] != token);
        if tokens.len() != before {
            self.save(&store);
        }
    }

    /// Validate an access token (JWT HS256 signed with its refresh token's `jwt_key`).
    pub fn validate(&self, jwt: &str) -> Option<User> {
        let mut parts = jwt.splitn(3, '.');
        let (header, payload, sig) = (parts.next()?, parts.next()?, parts.next()?);
        let claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
        let store = self.store.lock().unwrap();
        let data = &store["data"];
        let rt = find(data, "refresh_tokens", claims["iss"].as_str()?)?;
        let mut mac = Hmac::<Sha256>::new_from_slice(rt["jwt_key"].as_str()?.as_bytes()).ok()?;
        mac.update(format!("{header}.{payload}").as_bytes());
        mac.verify_slice(&URL_SAFE_NO_PAD.decode(sig).ok()?).ok()?;
        // PyJWT leeway=10
        if claims["exp"].as_f64()? + 10.0 < now_secs() {
            return None;
        }
        let user = find(data, "users", rt["user_id"].as_str()?)?;
        if user["is_active"] != true {
            return None;
        }
        Some(User {
            id: user["id"].as_str()?.into(),
            name: user["name"].as_str().unwrap_or_default().into(),
            is_owner: user["is_owner"] == true,
            is_admin: user["group_ids"]
                .as_array()
                .is_some_and(|g| g.iter().any(|g| g == "system-admin")),
        })
    }

    fn save(&self, store: &Value) {
        if let Err(e) = write_private(&self.path, store) {
            // The in-memory store stays authoritative; next save retries.
            eprintln!("saving {}: {e}", self.path.display());
        }
    }
}

fn find<'a>(data: &'a Value, list: &str, id: &str) -> Option<&'a Value> {
    data[list].as_array()?.iter().find(|v| v["id"] == id)
}

fn access_token(rt: &Value) -> String {
    let now = chrono::Utc::now().timestamp();
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD
        .encode(json!({"iss": rt["id"], "iat": now, "exp": now + ACCESS_TOKEN_SECS}).to_string());
    let mut mac =
        Hmac::<Sha256>::new_from_slice(rt["jwt_key"].as_str().unwrap_or_default().as_bytes())
            .unwrap();
    mac.update(format!("{header}.{payload}").as_bytes());
    format!(
        "{header}.{payload}.{}",
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    )
}

/// HA's rule (indieauth.verify_redirect_uri): same scheme and host as the client_id, or a known app pair.
pub fn redirect_ok(client_id: &str, redirect_uri: &str) -> bool {
    let origin = |u: &str| {
        let (scheme, rest) = u.split_once("://")?;
        let host = rest.split(['/', '?', '#']).next()?;
        matches!(scheme, "http" | "https").then(|| format!("{scheme}://{}", host.to_lowercase()))
    };
    // shortcut: only the frontend's same-origin form and the Companion apps; HA also fetches the
    // client_id page for <link rel=redirect_uri>. Upgrade if a third-party client needs it.
    matches!(
        (client_id, redirect_uri),
        (
            "https://home-assistant.io/iOS",
            "homeassistant://auth-callback"
        ) | (
            "https://home-assistant.io/android",
            "homeassistant://auth-callback"
        )
    ) || origin(client_id).is_some_and(|o| Some(o) == origin(redirect_uri))
}

/// HA's client_id check (indieauth.verify_client_id), minus the hostname rules for non-local IPs.
pub fn client_id_ok(client_id: &str) -> bool {
    let Some((scheme, rest)) = client_id.split_once("://") else {
        return false;
    };
    let netloc = rest.split(['/', '?', '#']).next().unwrap_or_default();
    matches!(scheme, "http" | "https")
        && !netloc.is_empty()
        && !netloc.contains('@')
        && !client_id.contains('#')
        && !rest
            .split(['?', '#'])
            .next()
            .unwrap_or_default()
            .split('/')
            .any(|s| s == "." || s == "..")
}

fn normalize(username: &str) -> String {
    username.trim().to_lowercase()
}

pub fn random_hex(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("OS random source");
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_micros() as f64 / 1e6
}

/// Python's `datetime.isoformat()` for an aware UTC datetime.
pub fn iso_now() -> String {
    chrono::Utc::now()
        .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
        .to_string()
}

/// HA's Store writes: atomic, owner-only (the file holds token secrets).
fn write_private(path: &Path, value: &Value) -> std::io::Result<()> {
    write_store(path, value, true)
}

/// A `.storage` file, as HA's Store writes it: atomic; `private` (0600) for secrets, else 0644.
pub(crate) fn write_store(path: &Path, value: &Value, private: bool) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("lean-home-tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(if private { 0o600 } else { 0o644 })
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec_pretty(value)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_backoff() {
        let g = LoginGuard::default();
        let ip: IpAddr = "192.0.2.1".parse().unwrap();
        for _ in 0..FREE_FAILURES {
            assert_eq!(g.delay(ip), Duration::ZERO);
            g.record(ip, false);
        }
        assert_eq!(g.delay(ip), Duration::from_secs(1));
        g.record(ip, false);
        assert_eq!(g.delay(ip), Duration::from_secs(2));
        for _ in 0..20 {
            g.record(ip, false);
        }
        assert_eq!(g.delay(ip), MAX_DELAY);
        // Other addresses are unaffected; a success clears the record.
        assert_eq!(g.delay("192.0.2.2".parse().unwrap()), Duration::ZERO);
        g.record(ip, true);
        assert_eq!(g.delay(ip), Duration::ZERO);
    }

    #[test]
    fn redirect_rules() {
        assert!(redirect_ok(
            "http://hub.local:8123/",
            "http://HUB.local:8123/?auth_callback=1"
        ));
        assert!(!redirect_ok(
            "http://hub.local:8123/",
            "http://evil.example/"
        ));
        assert!(!redirect_ok(
            "http://hub.local:8123/",
            "http://hub.local:8124/"
        ));
        assert!(redirect_ok(
            "https://home-assistant.io/iOS",
            "homeassistant://auth-callback"
        ));
        assert!(client_id_ok("http://127.0.0.1:8123/"));
        assert!(!client_id_ok("ftp://x/"));
        assert!(!client_id_ok("http://u@x/"));
        assert!(!client_id_ok("http://x/a/../b"));
    }
}
