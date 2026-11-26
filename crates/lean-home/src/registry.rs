//! HA's entity and device registries and config entries, read from
//! `.storage/core.{entity,device}_registry` and `.storage/core.config_entries`.
// shortcut: read-only, loaded at startup. Writing them comes when lean-home creates or edits
// entities itself (config UI).

use std::path::Path;

use serde_json::{Map, Value, json};

#[derive(Default)]
pub struct Registry {
    pub entities: Vec<Value>,
    pub devices: Vec<Value>,
    pub config_entries: Vec<Value>,
}

/// A stored ISO time as the epoch seconds HA's API sends (`datetime.timestamp()`).
fn timestamp(v: &Value) -> Value {
    v.as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map_or(Value::Null, |t| (t.timestamp_micros() as f64 / 1e6).into())
}

impl Registry {
    pub fn load(config_dir: &Path) -> Result<Self, String> {
        let read = |name: &str, list: &str| -> Result<Vec<Value>, String> {
            let p = config_dir.join(".storage").join(name);
            match std::fs::read_to_string(&p) {
                Ok(s) => {
                    let v: Value =
                        serde_json::from_str(&s).map_err(|e| format!("{}: {e}", p.display()))?;
                    Ok(v["data"][list].as_array().cloned().unwrap_or_default())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
                Err(e) => Err(format!("{}: {e}", p.display())),
            }
        };
        Ok(Self {
            entities: read("core.entity_registry", "entities")?,
            devices: read("core.device_registry", "devices")?,
            config_entries: read("core.config_entries", "entries")?,
        })
    }

    /// The registry entry HA keys by (domain, platform, unique_id).
    pub fn entity(&self, platform: &str, domain: &str, unique_id: Option<&str>) -> Option<&Value> {
        let uid = unique_id?;
        self.entities.iter().find(|e| {
            e["platform"] == platform
                && e["unique_id"] == uid
                && e["entity_id"]
                    .as_str()
                    .is_some_and(|id| id.starts_with(&format!("{domain}.")))
        })
    }

    /// The device whose identifiers match a discovery `device` block's.
    pub fn mqtt_device(&self, device: &Map<String, Value>) -> Option<&Value> {
        let ids: Vec<&str> = match device.get("identifiers") {
            Some(Value::String(s)) => vec![s.as_str()],
            Some(Value::Array(a)) => a.iter().filter_map(Value::as_str).collect(),
            _ => vec![],
        };
        self.devices.iter().find(|d| {
            d["identifiers"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|pair| pair[0] == "mqtt" && pair[1].as_str().is_some_and(|i| ids.contains(&i)))
        })
    }

    pub fn entity_id_taken(&self, entity_id: &str) -> bool {
        self.entities.iter().any(|e| e["entity_id"] == entity_id)
    }

    /// `config/entity_registry/list`: HA's `as_partial_dict` of every entry, disabled ones too.
    pub fn list_entities(&self) -> Vec<Value> {
        self.entities
            .iter()
            .map(|e| {
                let next = match (e["area_id"].is_null(), e["device_id"].is_null()) {
                    (false, _) => json!("area"),
                    (true, false) => json!("device"),
                    _ => Value::Null,
                };
                let original = if e["original_name_unprefixed"].is_null() {
                    &e["original_name"]
                } else {
                    &e["original_name_unprefixed"]
                };
                json!({
                    "area_id": e["area_id"], "categories": e.get("categories").cloned().unwrap_or(json!({})),
                    "config_entry_id": e["config_entry_id"], "config_subentry_id": e["config_subentry_id"],
                    "created_at": timestamp(&e["created_at"]), "device_id": e["device_id"],
                    "disabled_by": e["disabled_by"], "entity_category": e["entity_category"],
                    "entity_id": e["entity_id"], "hidden_by": e["hidden_by"], "icon": e["icon"], "id": e["id"],
                    "labels": e.get("labels").cloned().unwrap_or(json!([])),
                    "modified_at": timestamp(&e["modified_at"]), "name": e["name"], "next_name_part": next,
                    "options": e.get("options").cloned().unwrap_or(json!({})), "original_name": original,
                    "platform": e["platform"], "translation_key": e["translation_key"], "unique_id": e["unique_id"],
                })
            })
            .collect()
    }

    /// `config_entries/subscribe`'s first event: each entry as HA's `as_json_fragment`. `loaded`:
    /// the domains lean-home runs; the others are `not_loaded`.
    // shortcut: no options, reconfigure, unload or subentry flows yet, so every entry says it
    // supports none; `type_filter` is ignored (it needs every integration's manifest).
    pub fn config_entries_event(&self, loaded: &[&str]) -> Value {
        self.config_entries
            .iter()
            .map(|e| {
                let domain = e["domain"].as_str().unwrap_or_default();
                let disabled = !e["disabled_by"].is_null();
                let state = if loaded.contains(&domain) && !disabled {
                    "loaded"
                } else {
                    "not_loaded"
                };
                json!({"type": null, "entry": {
                    "created_at": timestamp(&e["created_at"]), "entry_id": e["entry_id"], "domain": domain,
                    "modified_at": timestamp(&e["modified_at"]), "title": e["title"], "source": e["source"],
                    "state": state, "supports_options": false, "supports_remove_device": false,
                    "supports_unload": false, "supports_reconfigure": false, "supported_subentry_types": {},
                    "pref_disable_new_entities": e["pref_disable_new_entities"].as_bool().unwrap_or(false),
                    "pref_disable_polling": e["pref_disable_polling"].as_bool().unwrap_or(false),
                    "disabled_by": e["disabled_by"], "reason": null, "error_reason_translation_domain": null,
                    "error_reason_translation_key": null, "error_reason_translation_placeholders": null,
                    "num_subentries": e["subentries"].as_array().map_or(0, Vec::len),
                }})
            })
            .collect()
    }

    /// `config/entity_registry/list_for_display`: HA's compact `_as_display_dict` per enabled entry.
    pub fn display_entities(&self) -> Vec<Value> {
        let s = |e: &Value, k: &str| e[k].as_str().map(String::from);
        self.entities
            .iter()
            .filter(|e| e["disabled_by"].is_null())
            .map(|e| {
                let mut d = serde_json::json!({"ei": e["entity_id"], "pl": e["platform"]});
                for (key, field) in [("ai", "area_id"), ("lb", "labels"), ("di", "device_id")] {
                    if !e[field].is_null() {
                        d[key] = e[field].clone();
                    }
                }
                let next = if !e["area_id"].is_null() {
                    Some("area")
                } else if !e["device_id"].is_null() {
                    Some("device")
                } else {
                    None
                };
                if let Some(n) = next {
                    d["np"] = n.into();
                }
                for (key, field) in [("ic", "icon"), ("tk", "translation_key")] {
                    if !e[field].is_null() {
                        d[key] = e[field].clone();
                    }
                }
                match e["entity_category"].as_str() {
                    Some("config") => d["ec"] = 0.into(),
                    Some("diagnostic") => d["ec"] = 1.into(),
                    _ => {}
                }
                if !e["hidden_by"].is_null() {
                    d["hb"] = true.into();
                }
                if let Some(n) = s(e, "name")
                    .or_else(|| s(e, "original_name_unprefixed"))
                    .or_else(|| s(e, "original_name"))
                    .filter(|n| !n.is_empty())
                {
                    d["en"] = n.into();
                }
                let opts = &e["options"]["sensor"];
                let dp = if opts["display_precision"].is_null() {
                    &opts["suggested_display_precision"]
                } else {
                    &opts["display_precision"]
                };
                if e["entity_id"]
                    .as_str()
                    .is_some_and(|i| i.starts_with("sensor."))
                    && !dp.is_null()
                {
                    d["dp"] = dp.clone();
                }
                d
            })
            .collect()
    }

    /// `config/device_registry/list`: HA's `DeviceEntry.dict_repr`, which differs from the file.
    // shortcut: plain devices only; composite devices (HA 2026.x `composite_*`) report extra entries.
    pub fn display_devices(&self) -> Vec<Value> {
        let ts = |v: &Value| {
            v.as_str()
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
                .map(|t| t.timestamp_micros() as f64 / 1e6)
        };
        self.devices
            .iter()
            .map(|d| {
                let entry = &d["config_entry_id"];
                let entries: Vec<&Value> = if entry.is_null() { vec![] } else { vec![entry] };
                let mut subentries = Map::new();
                if let Some(e) = entry.as_str() {
                    subentries.insert(e.into(), serde_json::json!([d["config_subentry_id"]]));
                }
                serde_json::json!({
                    "area_id": d["area_id"],
                    "configuration_url": d["configuration_url"],
                    "config_entries": entries,
                    "config_entries_subentries": subentries,
                    "config_entry_id": entry,
                    "config_subentry_id": d["config_subentry_id"],
                    "connections": d["connections"],
                    "created_at": ts(&d["created_at"]),
                    "disabled_by": d["disabled_by"],
                    "entry_type": d["entry_type"],
                    "hw_version": d["hw_version"],
                    "id": d["id"],
                    "identifiers": d["identifiers"],
                    "labels": d["labels"],
                    "manufacturer": d["manufacturer"],
                    "model": d["model"],
                    "model_id": d["model_id"],
                    "modified_at": ts(&d["modified_at"]),
                    "name_by_user": d["name_by_user"],
                    "name": d["name"],
                    "next_name_part": if d["area_id"].is_null() { Value::Null } else { "area".into() },
                    "parent_device_id": null,
                    "primary_config_entry": entry,
                    "serial_number": d["serial_number"],
                    "sw_version": d["sw_version"],
                    "via_device_id": d["via_device_id"],
                })
            })
            .collect()
    }
}
