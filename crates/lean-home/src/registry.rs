//! HA's entity and device registries, read from `.storage/core.{entity,device}_registry`.
// shortcut: read-only, loaded at startup. Writing them comes when lean-home creates or edits
// entities itself (config UI).

use std::path::Path;

use serde_json::{Map, Value};

#[derive(Default)]
pub struct Registry {
    pub entities: Vec<Value>,
    pub devices: Vec<Value>,
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
        })
    }

    /// The registry entry HA keys by (domain, platform "mqtt", unique_id).
    pub fn mqtt_entity(&self, domain: &str, unique_id: Option<&str>) -> Option<&Value> {
        let uid = unique_id?;
        self.entities.iter().find(|e| {
            e["platform"] == "mqtt"
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
