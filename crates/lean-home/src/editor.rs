//! The automation and script editors' REST API (HA's `components/config/{view,automation,
//! script}.py`): read one config from `automations.yaml`/`scripts.yaml`, save one (the whole file
//! rewritten as HA's dumper writes it), delete one, then reload what changed. The first place
//! lean-home writes a user's own files: every write is atomic and the previous file is kept as
//! `<file>.bak`.

use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;

use axum::http::StatusCode;
use serde_json::{Map, Value, json};

use crate::App;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    /// `automations.yaml`: a list, each entry keyed by its `id`.
    Automation,
    /// `scripts.yaml`: a mapping keyed by the script's key.
    Script,
}

impl Kind {
    fn file(self) -> &'static str {
        match self {
            Kind::Automation => "automations.yaml",
            Kind::Script => "scripts.yaml",
        }
    }
}

/// A reply: status and HA's `{"message": …}` or the result.
pub type Reply = (StatusCode, Value);

fn message(status: StatusCode, msg: &str) -> Reply {
    (status, json!({"message": msg}))
}

/// One read-modify-write at a time, as HA's view holds its `mutation_lock`.
static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The file's contents (HA's `read_config`): missing or empty is an empty list or mapping.
fn read(app: &App, kind: Kind) -> Result<Value, Reply> {
    let data = crate::config::load_file(&app.config_dir.join(kind.file()))
        .map_err(|e| message(StatusCode::INTERNAL_SERVER_ERROR, &e))?;
    let empty = match kind {
        Kind::Automation => json!([]),
        Kind::Script => json!({}),
    };
    match data {
        None | Some(Value::Null) => Ok(empty),
        Some(v) if v.is_array() == empty.is_array() && (v.is_array() || v.is_object()) => Ok(v),
        Some(_) => Err(message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!(
                "{}: expected {}",
                kind.file(),
                if kind == Kind::Automation {
                    "a list"
                } else {
                    "a mapping"
                }
            ),
        )),
    }
}

/// `GET`: the config as written in the file.
pub async fn get(app: &App, kind: Kind, key: &str) -> Reply {
    let _lock = LOCK.lock().await;
    let found = match read(app, kind) {
        Ok(Value::Array(list)) => list.into_iter().find(|a| a["id"] == key),
        Ok(Value::Object(mut scripts)) => scripts.remove(key),
        Ok(_) => None,
        Err(r) => return r,
    };
    match found {
        Some(v) => (StatusCode::OK, v),
        None => message(StatusCode::NOT_FOUND, "Resource not found"),
    }
}

/// `POST`: validate, write into the file, reload. HA answers before its reload finishes; here
/// the reload is done first, so the entity is there when the answer arrives.
pub async fn save(app: &Arc<App>, kind: Kind, key: &str, body: &[u8]) -> Reply {
    let Ok(Value::Object(new)) = serde_json::from_slice::<Value>(body) else {
        return match serde_json::from_slice::<Value>(body) {
            Err(_) => message(StatusCode::BAD_REQUEST, "Invalid JSON specified"),
            Ok(_) => message(
                StatusCode::BAD_REQUEST,
                "Message malformed: expected a dictionary",
            ),
        };
    };
    let slug = crate::mqtt::slugify(key);
    if kind == Kind::Script && slug != key {
        return message(
            StatusCode::BAD_REQUEST,
            &format!("Key malformed: invalid slug {key} (try {slug})"),
        );
    }
    if let Err(e) = validate(kind, &new) {
        return message(StatusCode::BAD_REQUEST, &format!("Message malformed: {e}"));
    }
    {
        let _lock = LOCK.lock().await;
        let mut data = match read(app, kind) {
            Ok(d) => d,
            Err(r) => return r,
        };
        match &mut data {
            Value::Array(list) => write_automation(list, key, new),
            Value::Object(scripts) => {
                scripts.insert(key.into(), new.into());
            }
            _ => {}
        }
        if let Err(r) = write(app, kind, &data) {
            return r;
        }
    }
    let reloaded = match kind {
        Kind::Automation => app.automations.reload(app, Some(key)).await,
        Kind::Script => app.scripts.reload(app).await,
    };
    if let Err(e) = reloaded {
        eprintln!(
            "config editor: saved {}, but reloading failed: {e}",
            kind.file()
        );
    }
    (StatusCode::OK, json!({"result": "ok"}))
}

/// `DELETE`: remove it from the file, and its entity (HA removes the registry entry).
pub async fn delete(app: &Arc<App>, kind: Kind, key: &str) -> Reply {
    {
        let _lock = LOCK.lock().await;
        let mut data = match read(app, kind) {
            Ok(d) => d,
            Err(r) => return r,
        };
        let found = match &mut data {
            Value::Array(list) => list
                .iter()
                .position(|a| a["id"] == key)
                .map(|i| list.remove(i))
                .is_some(),
            Value::Object(scripts) => scripts.shift_remove(key).is_some(),
            _ => false,
        };
        if !found {
            // HA answers 400 here, unlike a GET's 404.
            return message(StatusCode::BAD_REQUEST, "Resource not found");
        }
        if let Err(r) = write(app, kind, &data) {
            return r;
        }
    }
    match kind {
        Kind::Automation => app.automations.remove(app, key).await,
        Kind::Script => app.scripts.remove(app, key).await,
    }
    (StatusCode::OK, json!({"result": "ok"}))
}

/// What HA's schema validation would refuse outright.
// shortcut: only the required keys are checked; HA validates the whole schema and answers 400
// for any invalid option. Here such a config is saved, and its entity is unavailable with the
// error in the log (as for an unsupported option).
fn validate(kind: Kind, new: &Map<String, Value>) -> Result<(), String> {
    let required: &[&str] = match kind {
        Kind::Automation => &["triggers", "actions"],
        Kind::Script => &["sequence"],
    };
    for key in required {
        let legacy = key.trim_end_matches('s');
        if !new.contains_key(*key) && !(kind == Kind::Automation && new.contains_key(legacy)) {
            return Err(format!("required key not provided @ data['{key}']"));
        }
    }
    Ok(())
}

/// HA's `EditAutomationConfigView._write_value`: `id` first, then the editor's keys in a fixed
/// order, then the rest; replaces the entry with that id or appends. Entries without an `id`
/// get one, as HA does on every save.
fn write_automation(list: &mut Vec<Value>, key: &str, new: Map<String, Value>) {
    let mut value = Map::new();
    value.insert("id".into(), key.into());
    for k in [
        "alias",
        "description",
        "triggers",
        "trigger",
        "conditions",
        "condition",
        "actions",
        "action",
    ] {
        if let Some(v) = new.get(k) {
            value.insert(k.into(), v.clone());
        }
    }
    value.extend(new);
    let mut updated = false;
    for cur in list.iter_mut() {
        if let Value::Object(m) = cur {
            if !m.contains_key("id") {
                m.insert("id".into(), crate::auth::random_hex(16).into());
            } else if m["id"] == key {
                *cur = value.clone().into();
                updated = true;
            }
        }
    }
    if !updated {
        list.push(value.into());
    }
}

/// Dump and write the file atomically (temp file, fsync, rename), keeping the previous one as
/// `<file>.bak`.
fn write(app: &App, kind: Kind, data: &Value) -> Result<(), Reply> {
    let text = crate::yaml_dump::dump(data);
    write_atomic(&app.config_dir.join(kind.file()), &text).map_err(|e| {
        eprintln!("config editor: writing {} failed: {e}", kind.file());
        message(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Writing {} failed: {e}", kind.file()),
        )
    })
}

fn write_atomic(path: &Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let dir = path.parent().unwrap_or(Path::new("."));
    if path.exists() {
        std::fs::copy(path, dir.join(format!("{name}.bak")))?;
    }
    let tmp = dir.join(format!(".{name}.tmp"));
    let mut f = std::fs::File::create(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)?;
    // The rename itself is durable once the directory is synced.
    std::fs::File::open(dir)?.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scenario's saves and deletes, applied to its files as lean-home's loader reads them,
    /// write exactly what HA wrote (`compat/fixtures/automations/*.saved.yaml`).
    #[test]
    fn writes_what_ha_wrote() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat");
        let load = |f: &str| {
            crate::config::load_file(&root.join("automations").join(f))
                .unwrap()
                .unwrap()
        };
        let steps = std::process::Command::new("bash")
            .args(["-c", ". compat/mqtt-scenario.sh; automations_steps"])
            .current_dir(root.join(".."))
            .output()
            .unwrap();
        let steps: Vec<Value> = serde_json::from_slice(&steps.stdout).unwrap();
        let (mut autos, mut scripts) = (load("automations.yaml"), load("scripts.yaml"));
        for api in steps.iter().filter_map(|s| s["api"].as_array()) {
            let (method, path) = (api[0].as_str().unwrap(), api[1].as_str().unwrap());
            let key = path.rsplit('/').next().unwrap();
            let body = api.get(2).and_then(Value::as_object).cloned();
            match (method, path.contains("automation"), body) {
                ("POST", true, Some(b)) => write_automation(autos.as_array_mut().unwrap(), key, b),
                ("POST", false, Some(b)) => {
                    scripts
                        .as_object_mut()
                        .unwrap()
                        .insert(key.into(), b.into());
                }
                ("DELETE", true, _) => autos.as_array_mut().unwrap().retain(|a| a["id"] != key),
                ("DELETE", false, _) => {
                    scripts.as_object_mut().unwrap().shift_remove(key);
                }
                _ => {}
            }
        }
        let fixture =
            |f: &str| std::fs::read_to_string(root.join("fixtures/automations").join(f)).unwrap();
        assert_eq!(
            crate::yaml_dump::dump(&autos),
            fixture("automations.saved.yaml")
        );
        assert_eq!(
            crate::yaml_dump::dump(&scripts),
            fixture("scripts.saved.yaml")
        );
    }

    #[test]
    fn atomic_write_keeps_a_backup() {
        let dir = crate::e2e_tests::fixture_dir();
        let p = dir.join("x.yaml");
        write_atomic(&p, "a: 1\n").unwrap();
        assert!(!dir.join("x.yaml.bak").exists());
        write_atomic(&p, "a: 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a: 2\n");
        assert_eq!(
            std::fs::read_to_string(dir.join("x.yaml.bak")).unwrap(),
            "a: 1\n"
        );
        assert!(!dir.join(".x.yaml.tmp").exists());
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn write_value_like_ha() {
        let mut list = vec![
            json!({"alias": "no id"}),
            json!({"id": "1", "alias": "old", "mode": "single"}),
        ];
        let new = json!({"mode": "queued", "actions": [], "alias": "new", "triggers": []});
        write_automation(&mut list, "1", new.as_object().unwrap().clone());
        assert_eq!(list[0]["id"].as_str().unwrap().len(), 32, "an id is added");
        let keys: Vec<&String> = list[1].as_object().unwrap().keys().collect();
        assert_eq!(keys, ["id", "alias", "triggers", "actions", "mode"]);
        write_automation(&mut list, "2", Map::new());
        assert_eq!(list.len(), 3);
        assert!(
            validate(Kind::Automation, &Map::new())
                .unwrap_err()
                .contains("triggers")
        );
        assert!(
            validate(
                Kind::Automation,
                json!({"trigger": [], "action": []}).as_object().unwrap()
            )
            .is_ok()
        );
        assert!(
            validate(Kind::Script, &Map::new())
                .unwrap_err()
                .contains("sequence")
        );
    }
}
