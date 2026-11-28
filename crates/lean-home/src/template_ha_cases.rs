//! HA's own template test cases (`compat/fixtures/templates/cases.jsonl`, made by
//! `compat/template-cases.py` from HA's tests and rendered by HA itself) replayed through
//! lean-home. Cases using HA helpers lean-home doesn't have yet are counted, not compared.

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::*;
use crate::state::{Context, States};

/// The HA-specific globals, filters and tests lean-home implements; Jinja's own are always
/// compared, since minijinja has to match them too.
const IMPLEMENTED: &[&str] = &[
    "states",
    "filter:states",
    "is_state",
    "test:is_state",
    "state_attr",
    "filter:state_attr",
    "is_state_attr",
    "test:is_state_attr",
    "has_value",
    "filter:has_value",
    "test:has_value",
    "float",
    "int",
    "bool",
    "filter:bool",
    "is_number",
    "filter:is_number",
    "test:is_number",
    "iif",
    "filter:iif",
    "filter:is_defined",
    "now",
    "utcnow",
    "as_timestamp",
    "filter:as_timestamp",
    "timedelta",
    "as_timedelta",
    "filter:as_timedelta",
    "timedelta_string",
    "filter:timedelta_string",
    "filter:to_json",
    "filter:from_json",
];

/// Timestamps and ULIDs differ between any two runs (states are set at the real time, with new
/// context ids), like the WS fixtures' `<ts>` and `<ulid>`.
fn normalize(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = String::new();
    let mut i = 0;
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    while i < b.len() {
        // YYYY-MM-DD[ T]HH:MM:SS(.ffffff)?(+HH:MM)?
        if digits(i..i + 4)
            && b.get(i + 4) == Some(&b'-')
            && digits(i + 5..i + 7)
            && b.get(i + 7) == Some(&b'-')
            && digits(i + 8..i + 10)
            && matches!(b.get(i + 10), Some(b' ' | b'T'))
            && digits(i + 11..i + 13)
        {
            let mut j = i + 19;
            if b.get(j) == Some(&b'.') {
                j += 1;
                while b.get(j).is_some_and(u8::is_ascii_digit) {
                    j += 1;
                }
            }
            if matches!(b.get(j), Some(b'+' | b'-'))
                && digits(j + 1..j + 3)
                && b.get(j + 3) == Some(&b':')
            {
                j += 6;
            }
            out.push_str("<ts>");
            i = j.min(b.len());
            continue;
        }
        // a 26-char Crockford ULID
        let ulid = |c: &u8| c.is_ascii_digit() || (c.is_ascii_uppercase() && !b"ILOU".contains(c));
        if b.len() >= i + 26
            && b[i..i + 26].iter().all(ulid)
            && b.get(i + 26).is_none_or(|c| !c.is_ascii_alphanumeric())
            && (i == 0 || !b[i - 1].is_ascii_alphanumeric())
        {
            out.push_str("<ulid>");
            i += 26;
            continue;
        }
        let c = s[i..].chars().next().unwrap();
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// Cases that differ from HA on purpose, with why.
const KNOWN_GAPS: &[(&str, &str)] = &[
    // HA's TemplateState.__eq__ listens to the entity when compared; minijinja has no hook for
    // comparing an object with a string. The comparison is always false in HA (an object never
    // equals a string), so re-rendering on that entity can't change the result.
    (
        "test_init.py::test_async_render_to_info_with_branching",
        "__eq__ listener",
    ),
    (
        "test_init.py::test_async_render_to_info_with_complex_branching",
        "__eq__ listener",
    ),
];

/// What actually triggers a re-render, the way HA's `_event_triggers_rerender` and
/// `TrackStates` see it: everything under `all`; lifecycle listening only where no broader
/// listener already covers it.
fn effective(
    all: bool,
    entities: Vec<String>,
    domains: Vec<String>,
    time: bool,
    all_lc: bool,
    domains_lc: Vec<String>,
) -> Value {
    let mut entities = entities;
    entities.sort();
    let mut domains = domains;
    domains.sort();
    if all {
        return json!({"all": true, "time": time});
    }
    let mut domains_lc: Vec<String> = domains_lc
        .into_iter()
        .filter(|d| !domains.contains(d))
        .collect();
    domains_lc.sort();
    json!({"all": false, "time": time, "entities": entities, "domains": domains,
        "all_lifecycle": all_lc, "domains_lifecycle": if all_lc { vec![] } else { domains_lc }})
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|x| x.as_str().map(String::from))
        .collect()
}

fn uses(case: &Value, name: &str) -> bool {
    case["uses"]
        .as_array()
        .is_some_and(|u| u.iter().any(|n| n == name))
}

struct Outcome {
    test: String,
    template: String,
    diff: String,
}

fn replay(case: &Value) -> Result<(), String> {
    let states = Arc::new(States::default());
    for s in case["states"].as_array().into_iter().flatten() {
        let attrs = s["attributes"].as_object().cloned().unwrap_or_default();
        states.set(
            s["entity_id"].as_str().unwrap(),
            s["state"].as_str().unwrap(),
            attrs,
            Context::new(None),
        );
    }
    let tpl = case["template"].as_str().unwrap();
    let vars = if case["variables"].is_null() {
        json!({})
    } else {
        case["variables"].clone()
    };
    let ours = render(&states, tpl, &vars, false);
    let mut diffs = vec![];
    match (&ours, case.get("error")) {
        (Err(e), None) => diffs.push(format!("we error ({e}), HA gives {}", case["result"])),
        (Ok((text, _)), Some(e)) => diffs.push(format!("we give {text:?}, HA errors ({e})")),
        _ => {}
    }
    if let Ok((text, listeners)) = &ours {
        // HA's pick is random too, so only check that ours renders (we're past the error check).
        if uses(case, "filter:random") {
            return Ok(());
        }
        if let Some(want) = case["text"].as_str()
            && normalize(text) != normalize(want)
        {
            diffs.push(format!("text {text:?}, HA {want:?}"));
        }
        let want: Value = serde_json::from_str(&normalize(&case["result"].to_string())).unwrap();
        let got: Value = serde_json::from_str(&normalize(&result(tpl, text).to_string())).unwrap();
        let want = &want;
        // {"py": repr} results are Python objects JSON can't hold; the text check covers them.
        if case.get("error").is_none() && want.get("py").is_none() && got != *want {
            diffs.push(format!("result {got}, HA {want}"));
        }
        let l = &case["listeners"];
        let ours = effective(
            listeners.all,
            listeners.entities.clone(),
            listeners.domains.clone(),
            listeners.time,
            listeners.all_lifecycle,
            listeners.domains_lifecycle.clone(),
        );
        let theirs = effective(
            l["all"] == true,
            strings(&l["entities"]),
            strings(&l["domains"]),
            l["time"] == true,
            l["all_lifecycle"] == true,
            strings(&l["domains_lifecycle"]),
        );
        if case.get("error").is_none() && ours != theirs {
            diffs.push(format!("listeners {ours}, HA {theirs}"));
        }
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        Err(diffs.join("; "))
    }
}

#[test]
fn ha_template_cases() {
    *FROZEN_NOW.write().unwrap() = Some("2026-11-24T12:00:00Z".parse().unwrap());
    let cases: Vec<Value> = include_str!("../../../compat/fixtures/templates/cases.jsonl")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let mut compared = 0;
    let mut known = 0;
    let mut not_implemented: BTreeMap<String, usize> = BTreeMap::new();
    let mut failures: Vec<Outcome> = vec![];
    for case in &cases {
        let missing: Vec<&str> = case["ha_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|n| !IMPLEMENTED.contains(n))
            .collect();
        if !missing.is_empty() {
            for n in missing {
                *not_implemented.entry(n.to_string()).or_default() += 1;
            }
            continue;
        }
        compared += 1;
        let test = case["test"].as_str().unwrap_or_default();
        if let Err(diff) = replay(case) {
            if KNOWN_GAPS.iter().any(|(t, _)| *t == test) {
                known += 1;
                continue;
            }
            failures.push(Outcome {
                test: case["test"].as_str().unwrap_or_default().into(),
                template: case["template"].as_str().unwrap_or_default().into(),
                diff,
            });
        }
    }
    eprintln!(
        "HA template cases: {} total, {compared} compared, {} differ, {known} known gaps, {} skipped (HA helpers not implemented)",
        cases.len(),
        failures.len(),
        cases.len() - compared
    );
    for f in &failures {
        eprintln!("DIFF {} | {:?} | {}", f.test, f.template, f.diff);
    }
    eprintln!("not implemented: {not_implemented:?}");
    assert!(
        failures.is_empty(),
        "{} cases differ from HA",
        failures.len()
    );
}
