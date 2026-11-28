//! HA's restore state (`helpers/restore_state.py`): `.storage/core.restore_state` holds the last
//! state of every "restore" entity, so a restart brings back what the state machine can't
//! rebuild: an automation's on/off and `last_triggered`, a script's `last_triggered`. Read at
//! startup; written at startup, every 15 minutes and at shutdown, as HA does. Entries for
//! entities this run doesn't have (HA's other integrations, removed entities) are kept for 7 days,
//! so a config dir shared with HA loses nothing.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Value, json};

use crate::App;
use crate::state::{State, iso};

/// HA's `STATE_DUMP_INTERVAL`.
const DUMP_INTERVAL: Duration = Duration::from_secs(15 * 60);
/// HA's `STATE_EXPIRATION`.
const EXPIRATION: chrono::Duration = chrono::Duration::days(7);

pub struct Restore {
    path: PathBuf,
    /// Stored entries (HA's `StoredState.as_dict`) by registry id, or `entity:<id>` for an entity
    /// without a registry entry: from the file, and entities removed in this run.
    stored: Mutex<IndexMap<String, Value>>,
}

fn key(entity_id: &str, registry_id: Option<&str>) -> String {
    registry_id.map_or_else(|| format!("entity:{entity_id}"), String::from)
}

impl Restore {
    /// Read the file; a missing or unreadable one is an empty cache (HA logs and goes on).
    pub fn load(config_dir: &std::path::Path) -> Self {
        let path = config_dir.join(".storage/core.restore_state");
        let mut stored = IndexMap::new();
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    for item in v["data"].as_array().into_iter().flatten() {
                        let Some(eid) = item["state"]["entity_id"].as_str() else {
                            continue;
                        };
                        if !eid.contains('.') {
                            continue;
                        }
                        stored.insert(key(eid, item["entity_registry_id"].as_str()), item.clone());
                    }
                }
                Err(e) => eprintln!("restore state: {}: {e}", path.display()),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => eprintln!("restore state: {}: {e}", path.display()),
        }
        Self {
            path,
            stored: Mutex::new(stored),
        }
    }

    /// An entity's last stored state (`State.as_dict`): by its registry entry if it has one,
    /// else by entity id (HA's `async_get_stored_state`).
    pub fn last_state(&self, entity_id: &str, registry_id: Option<&str>) -> Option<Value> {
        self.stored
            .lock()
            .unwrap()
            .get(&key(entity_id, registry_id))
            .map(|s| s["state"].clone())
    }

    /// An entity is going away: keep its last state (HA's `async_restore_entity_removed`).
    pub fn removed(&self, state: Option<&State>, registry_id: Option<&str>) {
        if let Some(s) = state {
            let entry = json!({"state": s.json(), "extra_data": null, "last_seen": iso(Utc::now()),
                "entity_registry_id": registry_id});
            self.stored
                .lock()
                .unwrap()
                .insert(key(&s.entity_id, registry_id), entry);
        }
    }

    /// HA's `async_get_stored_states`, written as the file (`async_dump_states`).
    pub fn dump(&self, app: &App) {
        let now = Utc::now();
        let mut data = vec![];
        let mut current = vec![];
        // shortcut: lean-home's restore entities are its automations and scripts; HA's MQTT
        // entities (restore entities in HA, with sensor data in `extra_data`) keep the entry HA
        // last wrote until it expires. Add them if lean-home restores MQTT entities.
        for entity_id in app
            .automations
            .entity_ids()
            .into_iter()
            .chain(app.scripts.entity_ids())
        {
            let Some(s) = app
                .states
                .get(&entity_id)
                .filter(|s| !s.attributes.contains_key("restored"))
            else {
                continue;
            };
            let reg = app.registry.entry_id(&entity_id);
            current.push(key(&entity_id, reg.as_deref()));
            data.push(
                json!({"state": s.json(), "extra_data": null, "last_seen": iso(now),
                "entity_registry_id": reg}),
            );
        }
        let expired = |e: &Value| {
            e["last_seen"]
                .as_str()
                .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
                .is_none_or(|t| t < now - EXPIRATION)
        };
        for (k, e) in self.stored.lock().unwrap().iter() {
            let live = match k.strip_prefix("entity:") {
                // Without a registry entry: any state of that entity in this run replaces it.
                Some(eid) => app
                    .states
                    .get(eid)
                    .is_some_and(|s| !s.attributes.contains_key("restored")),
                None => current.contains(k),
            };
            if !live && !expired(e) {
                data.push(e.clone());
            }
        }
        let store =
            json!({"version": 1, "minor_version": 1, "key": "core.restore_state", "data": data});
        if let Err(e) = crate::auth::write_store(&self.path, &store, false) {
            eprintln!("restore state: saving {}: {e}", self.path.display());
        }
    }
}

/// Dump now and every 15 minutes (HA starts its dump at startup, after the old states were
/// read). The dump at shutdown is in `main`.
pub fn start(app: &std::sync::Arc<App>) {
    let app = app.clone();
    tokio::spawn(async move {
        let mut every = tokio::time::interval(DUMP_INTERVAL);
        loop {
            every.tick().await;
            app.restore.dump(&app);
        }
    });
}

/// What an automation or script takes from its stored state: `last_triggered`, and for an
/// automation whether it was on.
pub fn parse(state: &Value) -> (Option<DateTime<Utc>>, Option<bool>) {
    let last = state["attributes"]["last_triggered"]
        .as_str()
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    let on = state["state"].as_str().map(|s| s == "on");
    (last, on)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;

    /// HA's file from the automations recording, restored: on/off and `last_triggered` come
    /// back; entries for entities lean-home doesn't have are written back untouched.
    #[test]
    fn restores_and_keeps_like_ha() {
        let dir = crate::e2e_tests::fixture_dir();
        let cfg = dir.join("config");
        let compat = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat");
        // The fixture with real values for its placeholders, as make-config.py writes them.
        let fixture =
            std::fs::read_to_string(compat.join("fixtures/automations/core.restore_state.json"))
                .unwrap()
                .replace(
                    "\"<ts>\"",
                    &format!("\"{}\"", iso(Utc::now() - chrono::Duration::hours(1))),
                )
                .replace("<hex32>", "fedcba9876543210fedcba9876543210")
                .replace("<ulid>", "01KAQ0000000000000000000ZZ");
        // The recording's registry ids all became one placeholder: key by entity id instead,
        // as HA does for an entity without a registry entry (this config has no registry).
        let mut fixture: Value = serde_json::from_str(&fixture).unwrap();
        for e in fixture["data"].as_array_mut().unwrap() {
            e["entity_registry_id"] = Value::Null;
        }
        let fixture = fixture.to_string();
        std::fs::write(cfg.join(".storage/core.restore_state"), &fixture).unwrap();
        for f in ["automations.yaml", "scripts.yaml"] {
            std::fs::copy(compat.join("automations").join(f), cfg.join(f)).unwrap();
        }
        std::fs::write(
            cfg.join("configuration.yaml"),
            "automation: !include automations.yaml\nscript: !include scripts.yaml\n",
        )
        .unwrap();
        let app = App::load(&cfg, &dir.join("frontend"), false).unwrap();
        // nightly_off was turned off in the recording; the others were on and had run
        let st = |id: &str| app.states.get(id).unwrap();
        assert_eq!(st("automation.nightly_off").state, "off");
        assert_eq!(st("automation.fan_lights_ceiling").state, "on");
        assert!(st("automation.fan_lights_ceiling").attributes["last_triggered"].is_string());
        assert!(st("automation.nightly_off").attributes["last_triggered"].is_null());
        assert!(st("script.blink").attributes["last_triggered"].is_string());

        app.states
            .set("script.blink", "on", Default::default(), Context::new(None));
        app.restore.dump(&app);
        let written: Value = serde_json::from_str(
            &std::fs::read_to_string(cfg.join(".storage/core.restore_state")).unwrap(),
        )
        .unwrap();
        let ids = |v: &Value| -> Vec<String> {
            let mut ids: Vec<String> = v["data"]
                .as_array()
                .unwrap()
                .iter()
                .map(|e| e["state"]["entity_id"].as_str().unwrap().to_string())
                .collect();
            ids.sort();
            ids
        };
        let before: Value = serde_json::from_str(&fixture).unwrap();
        assert_eq!(ids(&written), ids(&before), "every entity is still there");
        let entry = |v: &Value, id: &str| {
            v["data"]
                .as_array()
                .unwrap()
                .iter()
                .find(|e| e["state"]["entity_id"] == id)
                .unwrap()
                .clone()
        };
        assert_eq!(
            entry(&written, "script.blink")["state"]["state"],
            "on",
            "current state written"
        );
        assert_eq!(
            entry(&written, "person.dev"),
            entry(&before, "person.dev"),
            "others untouched"
        );
        assert_eq!(written["key"], "core.restore_state");
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn expired_and_removed() {
        let dir = crate::e2e_tests::fixture_dir();
        let old = json!({"version": 1, "minor_version": 1, "key": "core.restore_state", "data": [
            {"state": {"entity_id": "light.old", "state": "on", "attributes": {}}, "extra_data": null,
             "last_seen": iso(Utc::now() - chrono::Duration::days(8)), "entity_registry_id": null}]});
        std::fs::write(
            dir.join("config/.storage/core.restore_state"),
            old.to_string(),
        )
        .unwrap();
        let app = App::load(&dir.join("config"), &dir.join("frontend"), false).unwrap();
        let s = State {
            entity_id: "script.gone".into(),
            state: "off".into(),
            attributes: Default::default(),
            last_changed: Utc::now(),
            last_reported: Utc::now(),
            last_updated: Utc::now(),
            context: Context::new(None),
        };
        app.restore.removed(Some(&s), Some("abc"));
        assert_eq!(
            app.restore.last_state("script.gone", Some("abc")).unwrap()["state"],
            "off"
        );
        app.restore.dump(&app);
        let written =
            std::fs::read_to_string(dir.join("config/.storage/core.restore_state")).unwrap();
        assert!(!written.contains("light.old"), "expired after 7 days");
        assert!(written.contains("script.gone"), "a removed entity is kept");
        // With a registry entry, only the registry id finds it (an entity id isn't enough).
        assert!(app.restore.last_state("script.gone", None).is_none());
        std::fs::remove_dir_all(dir).ok();
    }
}
