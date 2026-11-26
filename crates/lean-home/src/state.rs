//! State machine and bus: `Arc<State>` per entity, a change swaps the pointer and is broadcast.

use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};
use tokio::sync::broadcast;

#[derive(Clone, Debug, PartialEq)]
pub struct Context {
    pub id: String,
    pub parent_id: Option<String>,
    pub user_id: Option<String>,
}

impl Context {
    pub fn new(user_id: Option<String>) -> Self {
        Self {
            id: ulid(),
            parent_id: None,
            user_id,
        }
    }

    pub fn json(&self) -> Value {
        json!({"id": self.id, "parent_id": self.parent_id, "user_id": self.user_id})
    }
}

#[derive(Debug)]
pub struct State {
    pub entity_id: Arc<str>,
    pub state: String,
    pub attributes: Map<String, Value>,
    pub last_changed: DateTime<Utc>,
    pub last_reported: DateTime<Utc>,
    pub last_updated: DateTime<Utc>,
    pub context: Context,
}

/// A `state_changed` event.
#[derive(Debug)]
pub struct Change {
    pub entity_id: Arc<str>,
    pub old: Option<Arc<State>>,
    pub new: Option<Arc<State>>,
    pub time_fired: DateTime<Utc>,
    pub context: Context,
}

/// A general HA event (`homeassistant.core.Event`).
#[derive(Clone, Debug)]
pub struct Event {
    pub event_type: String,
    pub data: Map<String, Value>,
    pub origin: String,
    pub time_fired: DateTime<Utc>,
    pub context: Context,
}

impl Event {
    /// `Event.as_dict()`
    pub fn json(&self) -> Value {
        json!({
            "event_type": self.event_type,
            "data": self.data,
            "origin": self.origin,
            "time_fired": iso(self.time_fired),
            "context": self.context.json(),
        })
    }
}

#[derive(Debug)]
pub struct States {
    map: Mutex<IndexMap<Arc<str>, Arc<State>>>,
    bus: broadcast::Sender<Arc<Change>>,
    events: broadcast::Sender<Arc<Event>>,
}

impl Default for States {
    fn default() -> Self {
        // Capacity bounds how far a slow client may fall behind before it is disconnected.
        Self {
            map: Mutex::default(),
            bus: broadcast::channel(1024).0,
            events: broadcast::channel(1024).0,
        }
    }
}

impl States {
    pub fn set(
        &self,
        entity_id: &str,
        state: &str,
        attributes: Map<String, Value>,
        context: Context,
    ) {
        let mut map = self.map.lock().unwrap();
        let old = map.get(entity_id).cloned();
        let now = Utc::now();
        // HA fires no state_changed when nothing changed (only state_reported, which updates
        // last_reported without emitting a state_changed event).
        if let Some(o) = old
            .as_ref()
            .filter(|o| o.state == state && o.attributes == attributes)
        {
            let reported = Arc::new(State {
                entity_id: o.entity_id.clone(),
                state: o.state.clone(),
                attributes: o.attributes.clone(),
                last_changed: o.last_changed,
                last_reported: now,
                last_updated: o.last_updated,
                context: o.context.clone(),
            });
            map.insert(o.entity_id.clone(), reported);
            return;
        }
        let entity_id: Arc<str> = old
            .as_ref()
            .map_or_else(|| entity_id.into(), |o| o.entity_id.clone());
        let new = Arc::new(State {
            entity_id: entity_id.clone(),
            state: state.into(),
            attributes,
            last_changed: old
                .as_ref()
                .filter(|o| o.state == state)
                .map_or(now, |o| o.last_changed),
            last_reported: now,
            last_updated: now,
            context: context.clone(),
        });
        map.insert(entity_id.clone(), new.clone());
        // Sent under the lock so subscribers see changes in the same order as the map.
        let _ = self.bus.send(Arc::new(Change {
            entity_id,
            old,
            new: Some(new),
            time_fired: now,
            context,
        }));
    }

    pub fn remove(&self, entity_id: &str) {
        let mut map = self.map.lock().unwrap();
        if let Some(old) = map.shift_remove(entity_id) {
            let change = Change {
                entity_id: old.entity_id.clone(),
                old: Some(old),
                new: None,
                time_fired: Utc::now(),
                context: Context::new(None),
            };
            let _ = self.bus.send(Arc::new(change));
        }
    }

    pub fn get(&self, entity_id: &str) -> Option<Arc<State>> {
        self.map.lock().unwrap().get(entity_id).cloned()
    }

    pub fn all(&self) -> Vec<Arc<State>> {
        self.map.lock().unwrap().values().cloned().collect()
    }

    /// A bus receiver plus a snapshot taken atomically with it: no change is missed or doubled.
    pub fn subscribe(&self) -> (broadcast::Receiver<Arc<Change>>, Vec<Arc<State>>) {
        let map = self.map.lock().unwrap();
        (self.bus.subscribe(), map.values().cloned().collect())
    }

    /// Fire a general event on the bus (`hass.bus.async_fire`).
    pub fn fire_event(
        &self,
        event_type: impl Into<String>,
        data: Map<String, Value>,
        context: Context,
    ) -> Arc<Event> {
        let ev = Arc::new(Event {
            event_type: event_type.into(),
            data,
            origin: "LOCAL".into(),
            time_fired: Utc::now(),
            context,
        });
        let _ = self.events.send(ev.clone());
        ev
    }

    /// Subscribe to general events on the bus.
    pub fn subscribe_events(&self) -> broadcast::Receiver<Arc<Event>> {
        self.events.subscribe()
    }
}

/// Python's `datetime.isoformat()` for UTC.
pub fn iso(t: DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string()
}

fn ts(t: DateTime<Utc>) -> f64 {
    t.timestamp_micros() as f64 / 1e6
}

impl State {
    /// `State.as_dict()`
    pub fn json(&self) -> Value {
        let lc = iso(self.last_changed);
        let lr = iso(self.last_reported);
        let lu = iso(self.last_updated);
        json!({
            "entity_id": &*self.entity_id,
            "state": self.state,
            "attributes": self.attributes,
            "last_changed": lc,
            "last_reported": lr,
            "last_updated": lu,
            "context": self.context.json(),
        })
    }

    /// `State.as_compressed_state` (subscribe_entities)
    pub fn compressed(&self) -> Value {
        let c = &self.context;
        let context = if c.parent_id.is_none() && c.user_id.is_none() {
            json!(c.id)
        } else {
            c.json()
        };
        let mut v = json!({"s": self.state, "a": self.attributes, "c": context, "lc": ts(self.last_changed)});
        if self.last_changed != self.last_updated {
            v["lu"] = ts(self.last_updated).into();
        }
        v
    }
}

impl Change {
    /// `_state_diff_event`: the subscribe_entities event body.
    pub fn diff(&self) -> Value {
        let (Some(new), Some(old)) = (&self.new, &self.old) else {
            return match &self.new {
                None => json!({"r": [&*self.entity_id]}),
                Some(new) => json!({"a": {&*self.entity_id: new.compressed()}}),
            };
        };
        let mut add = Map::new();
        if old.state != new.state {
            add.insert("s".into(), new.state.clone().into());
        }
        if old.last_changed != new.last_changed {
            add.insert("lc".into(), ts(new.last_changed).into());
        } else if old.last_updated != new.last_updated {
            add.insert("lu".into(), ts(new.last_updated).into());
        }
        let (oc, nc) = (&old.context, &new.context);
        let mut ctx = Map::new();
        if oc.parent_id != nc.parent_id {
            ctx.insert("parent_id".into(), json!(nc.parent_id));
        }
        if oc.user_id != nc.user_id {
            ctx.insert("user_id".into(), json!(nc.user_id));
        }
        if oc.id != nc.id {
            if ctx.is_empty() {
                add.insert("c".into(), nc.id.clone().into());
            } else {
                ctx.insert("id".into(), nc.id.clone().into());
            }
        }
        if !ctx.is_empty() {
            add.insert("c".into(), ctx.into());
        }
        let changed: Map<_, _> = new
            .attributes
            .iter()
            .filter(|(k, v)| old.attributes.get(*k) != Some(v))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !changed.is_empty() {
            add.insert("a".into(), changed.into());
        }
        let mut diff = json!({"+": add});
        let removed: Vec<_> = old
            .attributes
            .keys()
            .filter(|k| !new.attributes.contains_key(*k))
            .collect();
        if !removed.is_empty() {
            diff["-"] = json!({"a": removed});
        }
        json!({"c": {&*self.entity_id: diff}})
    }

    /// `Event.as_dict()` for state_changed (subscribe_events)
    pub fn event(&self) -> Value {
        json!({
            "event_type": "state_changed",
            "data": {
                "entity_id": &*self.entity_id,
                "old_state": self.old.as_ref().map(|s| s.json()),
                "new_state": self.new.as_ref().map(|s| s.json()),
            },
            "origin": "LOCAL",
            "time_fired": iso(self.time_fired),
            "context": self.context.json(),
        })
    }
}

/// ULID, as HA uses for context ids: 48-bit ms timestamp + 80 random bits, Crockford base32.
pub fn ulid() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let mut rand = [0u8; 10];
    getrandom::fill(&mut rand).expect("OS random source");
    let mut n = (Utc::now().timestamp_millis() as u128) << 80;
    for (i, b) in rand.iter().enumerate() {
        n |= (*b as u128) << (72 - 8 * i);
    }
    (0..26)
        .map(|i| ALPHABET[((n >> (125 - 5 * i)) & 31) as usize] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn diff_matches_ha_shape() {
        let s = States::default();
        let (mut rx, _) = s.subscribe();
        s.set(
            "light.x",
            "off",
            attrs(json!({"friendly_name": "X", "color_mode": null})),
            Context::new(None),
        );
        let add = rx.try_recv().unwrap().diff();
        assert_eq!(add["a"]["light.x"]["s"], "off");
        assert!(add["a"]["light.x"]["c"].is_string());

        let ctx = Context::new(Some("u1".into()));
        s.set(
            "light.x",
            "on",
            attrs(json!({"friendly_name": "X", "color_mode": "onoff"})),
            ctx.clone(),
        );
        let d = rx.try_recv().unwrap().diff();
        let plus = &d["c"]["light.x"]["+"];
        assert_eq!(plus["s"], "on");
        assert_eq!(plus["c"], json!({"user_id": "u1", "id": ctx.id}));
        assert_eq!(plus["a"], json!({"color_mode": "onoff"}));
        assert!(plus["lc"].is_f64() && plus.get("lu").is_none());

        // Unchanged state: no event, but last_reported advances.
        let before = s.get("light.x").unwrap();
        s.set(
            "light.x",
            "on",
            attrs(json!({"friendly_name": "X", "color_mode": "onoff"})),
            Context::new(None),
        );
        assert!(rx.try_recv().is_err());
        let after = s.get("light.x").unwrap();
        assert_eq!(after.last_changed, before.last_changed);
        assert_eq!(after.last_updated, before.last_updated);
        assert!(after.last_reported >= before.last_reported);

        s.set(
            "light.x",
            "on",
            attrs(json!({"friendly_name": "X"})),
            Context::new(None),
        );
        let d = rx.try_recv().unwrap().diff();
        assert_eq!(d["c"]["light.x"]["-"], json!({"a": ["color_mode"]}));
        assert!(d["c"]["light.x"]["+"]["lu"].is_f64());
    }

    #[test]
    fn ulid_shape() {
        let (a, b) = (ulid(), ulid());
        assert_eq!(a.len(), 26);
        assert_ne!(a, b);
        assert!(a.starts_with('0')); // the top bits of a 48-bit timestamp are zero until year 10889
    }
}
