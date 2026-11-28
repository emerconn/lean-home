//! Script and automation run traces, as HA's `trace` integration keeps them (`helpers/trace.py`,
//! `components/trace/models.py`): one trace per run, elements keyed by their path in the config
//! (`sequence/2/choose/0/conditions/0`), the last 5 runs per item, served by `trace/list` and
//! `trace/get`.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde_json::{Map, Value, json};

use crate::state::{Context, iso};

/// HA's `DEFAULT_STORED_TRACES`.
const STORED_TRACES: usize = 5;

/// One step or condition evaluation (HA's `TraceElement`).
#[derive(Debug, Clone)]
pub struct Element {
    path: String,
    timestamp: DateTime<Utc>,
    changed_variables: Option<Map<String, Value>>,
    result: Option<Map<String, Value>>,
    error: Option<String>,
    child: Option<(String, String, String)>,
}

impl Element {
    fn json(&self) -> Value {
        let mut v = json!({"path": self.path, "timestamp": iso(self.timestamp)});
        if let Some((domain, item_id, run_id)) = &self.child {
            v["child_id"] = json!({"domain": domain, "item_id": item_id, "run_id": run_id});
        }
        if let Some(cv) = self.changed_variables.as_ref().filter(|m| !m.is_empty()) {
            v["changed_variables"] = cv.clone().into();
        }
        if let Some(e) = &self.error {
            v["error"] = e.clone().into();
        }
        if let Some(r) = &self.result {
            v["result"] = r.clone().into();
        }
        v
    }
}

/// One run (HA's `ActionTrace`).
#[derive(Debug)]
pub struct Trace {
    domain: &'static str,
    item_id: String,
    pub run_id: String,
    config: Value,
    blueprint_inputs: Option<Value>,
    context: Context,
    start: DateTime<Utc>,
    finish: Option<DateTime<Utc>>,
    script_execution: Option<String>,
    error: Option<String>,
    /// An automation's trigger description (`None` inside for a run without one); scripts have
    /// no `trigger` key.
    trigger: Option<Option<String>>,
    elements: IndexMap<String, Vec<Element>>,
}

impl Trace {
    fn short(&self) -> Value {
        json!({
            "last_step": self.elements.keys().last(),
            "run_id": self.run_id,
            "state": if self.finish.is_some() { "stopped" } else { "running" },
            "script_execution": self.script_execution,
            "timestamp": {"start": iso(self.start), "finish": self.finish.map(iso)},
            "domain": self.domain,
            "item_id": self.item_id,
        })
        .as_object()
        .cloned()
        .map(|mut m| {
            if let Some(e) = &self.error {
                m.insert("error".into(), e.clone().into());
            }
            if let Some(t) = &self.trigger {
                m.insert("trigger".into(), t.clone().into());
            }
            m.into()
        })
        .unwrap_or_default()
    }

    fn extended(&self) -> Value {
        let mut v = self.short();
        let trace: Map<String, Value> = self
            .elements
            .iter()
            .map(|(k, els)| (k.clone(), els.iter().map(Element::json).collect()))
            .collect();
        v["trace"] = trace.into();
        v["config"] = self.config.clone();
        v["blueprint_inputs"] = self.blueprint_inputs.clone().unwrap_or(Value::Null);
        v["context"] = self.context.json();
        v
    }
}

/// The trace a run writes to while it executes. Elements are appended under the current path;
/// `changed_variables` holds what changed since the previous element, as HA records it.
#[derive(Clone)]
pub struct Recorder {
    trace: Arc<Mutex<Trace>>,
    seen_vars: Arc<Mutex<Map<String, Value>>>,
}

/// Where an element sits in the trace: (path key, index in that path's list).
pub type ElementRef = (String, usize);

impl Recorder {
    /// Append an element at `path`, recording the variables that changed since the last one.
    pub fn element(&self, path: &str, variables: Option<&Map<String, Value>>) -> ElementRef {
        let changed = variables.map(|vars| {
            let mut seen = self.seen_vars.lock().unwrap();
            let changed: Map<String, Value> = vars
                .iter()
                .filter(|(k, v)| seen.get(*k) != Some(v))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            seen.extend(changed.clone());
            changed
        });
        let mut t = self.trace.lock().unwrap();
        let list = t.elements.entry(path.to_string()).or_default();
        list.push(Element {
            path: path.to_string(),
            timestamp: Utc::now(),
            changed_variables: changed,
            result: None,
            error: None,
            child: None,
        });
        (path.to_string(), list.len() - 1)
    }

    fn with(&self, at: &ElementRef, f: impl FnOnce(&mut Element)) {
        let mut t = self.trace.lock().unwrap();
        if let Some(e) = t.elements.get_mut(&at.0).and_then(|l| l.get_mut(at.1)) {
            f(e);
        }
    }

    /// HA's `trace_set_result`: replace the element's result.
    pub fn set_result(&self, at: &ElementRef, result: Value) {
        self.with(at, |e| e.result = result.as_object().cloned());
    }

    /// HA's `trace_update_result`: merge into the element's result.
    pub fn update_result(&self, at: &ElementRef, result: Value) {
        self.with(at, |e| {
            let r = e.result.get_or_insert_with(Map::new);
            if let Some(m) = result.as_object() {
                r.extend(m.clone());
            }
        });
    }

    pub fn set_error(&self, at: &ElementRef, error: &str) {
        self.with(at, |e| e.error = Some(error.to_string()));
    }

    /// The run a step started (a script it called), linked as `child_id`.
    pub fn set_child(&self, at: &ElementRef, domain: &str, item_id: &str, run_id: &str) {
        self.with(at, |e| {
            e.child = Some((domain.into(), item_id.into(), run_id.into()))
        });
    }

    /// An automation run's trigger description, shown as `trigger` in `trace/list`.
    pub fn set_trigger(&self, description: Option<String>) {
        self.trace.lock().unwrap().trigger = Some(description);
    }

    /// The original `use_blueprint` config for a blueprinted run.
    pub fn set_blueprint_inputs(&self, inputs: Option<Value>) {
        self.trace.lock().unwrap().blueprint_inputs = inputs;
    }

    pub fn run_id(&self) -> String {
        self.trace.lock().unwrap().run_id.clone()
    }

    /// The run ended: `script_execution` is HA's outcome (`finished`, `aborted`, `error`…).
    pub fn finish(&self, script_execution: &str, error: Option<String>) {
        let mut t = self.trace.lock().unwrap();
        t.finish = Some(Utc::now());
        t.script_execution = Some(script_execution.into());
        t.error = error;
    }
}

/// One item's stored runs, oldest first.
type Runs = VecDeque<Arc<Mutex<Trace>>>;

/// The stored traces of every script and automation, newest last.
#[derive(Default)]
pub struct Traces {
    items: Mutex<IndexMap<(String, String), Runs>>,
}

impl Traces {
    /// Start a trace for a run; the oldest is dropped beyond HA's 5 per item.
    pub fn start(
        &self,
        domain: &'static str,
        item_id: &str,
        config: Value,
        context: Context,
    ) -> Recorder {
        let mut run_id = [0u8; 16];
        getrandom::fill(&mut run_id).expect("OS random source");
        let trace = Arc::new(Mutex::new(Trace {
            domain,
            item_id: item_id.into(),
            run_id: run_id.iter().map(|b| format!("{b:02x}")).collect(),
            config,
            blueprint_inputs: None,
            context,
            start: Utc::now(),
            finish: None,
            script_execution: None,
            error: None,
            trigger: None,
            elements: IndexMap::new(),
        }));
        let mut items = self.items.lock().unwrap();
        let list = items.entry((domain.into(), item_id.into())).or_default();
        list.push_back(trace.clone());
        while list.len() > STORED_TRACES {
            list.pop_front();
        }
        Recorder {
            trace,
            seen_vars: Default::default(),
        }
    }

    /// `trace/list`: short dicts, oldest first; for one item or every item of the domain.
    pub fn list(&self, domain: &str, item_id: Option<&str>) -> Vec<Value> {
        let items = self.items.lock().unwrap();
        items
            .iter()
            .filter(|((d, i), _)| d == domain && item_id.is_none_or(|x| x == i))
            .flat_map(|(_, list)| list.iter().map(|t| t.lock().unwrap().short()))
            .collect()
    }

    /// `trace/get`
    pub fn get(&self, domain: &str, item_id: &str, run_id: &str) -> Option<Value> {
        let items = self.items.lock().unwrap();
        items
            .get(&(domain.to_string(), item_id.to_string()))?
            .iter()
            .find(|t| t.lock().unwrap().run_id == run_id)
            .map(|t| t.lock().unwrap().extended())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_like_ha() {
        let traces = Traces::default();
        let ctx = Context::new(None);
        for _ in 0..7 {
            traces
                .start("script", "s", json!({"sequence": []}), ctx.clone())
                .finish("finished", None);
        }
        assert_eq!(traces.list("script", Some("s")).len(), 5);

        let rec = traces.start("script", "s", json!({}), ctx);
        let mut vars = Map::new();
        vars.insert("x".into(), json!(1));
        let a = rec.element("sequence/0", Some(&vars));
        rec.set_result(&a, json!({"delay": 1.0, "done": false}));
        rec.update_result(&a, json!({"done": true}));
        // unchanged variables aren't repeated
        let b = rec.element("sequence/1", Some(&vars));
        rec.set_error(&b, "boom");
        let run = rec.run_id();
        assert_eq!(
            traces.list("script", Some("s")).last().unwrap()["state"],
            "running"
        );
        rec.finish("error", Some("boom".into()));
        let t = traces.get("script", "s", &run).unwrap();
        assert_eq!(t["state"], "stopped");
        assert_eq!(t["last_step"], "sequence/1");
        assert_eq!(t["error"], "boom");
        assert_eq!(
            t["trace"]["sequence/0"][0]["changed_variables"],
            json!({"x": 1})
        );
        assert_eq!(
            t["trace"]["sequence/0"][0]["result"],
            json!({"delay": 1.0, "done": true})
        );
        assert!(
            t["trace"]["sequence/1"][0]
                .get("changed_variables")
                .is_none()
        );
        assert_eq!(t["trace"]["sequence/1"][0]["error"], "boom");
        assert!(traces.get("script", "s", "nope").is_none());
    }
}
