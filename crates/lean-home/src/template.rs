//! HA's template engine on minijinja: the `states` object and HA's helpers (`homeassistant/helpers/
//! template/`), Python-style output, result parsing, and the listeners a render touched, which
//! `render_template` re-renders on.

use std::fmt::Write as _;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use chrono::{DateTime, FixedOffset, Utc};
use minijinja::value::{Enumerator, Kwargs, Object, ObjectRepr, Rest, Value};
use minijinja::{Environment, Error, ErrorKind, State as Vm, UndefinedBehavior};
use serde_json::json;

use crate::config::Zone;
use crate::state::{self, States};

/// The configured time zone, process-wide like HA's `dt_util.DEFAULT_TIME_ZONE`.
static TIME_ZONE: RwLock<Option<Zone>> = RwLock::new(None);

pub fn set_time_zone(tz: Zone) {
    *TIME_ZONE.write().unwrap() = Some(tz);
}

fn local_tz() -> Zone {
    TIME_ZONE.read().unwrap().clone().unwrap_or_else(Zone::utc)
}

/// What a render read, so it can be re-rendered when that changes (HA's `RenderInfo`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Listeners {
    pub all: bool,
    pub domains: Vec<String>,
    pub entities: Vec<String>,
    pub time: bool,
}

impl Listeners {
    fn entity(&mut self, id: &str) {
        if !self.entities.iter().any(|e| e == id) {
            self.entities.push(id.into());
        }
    }

    fn domain(&mut self, d: &str) {
        if !self.domains.iter().any(|e| e == d) {
            self.domains.push(d.into());
        }
    }

    /// Does this state change need a re-render?
    pub fn matches(&self, entity_id: &str) -> bool {
        self.all
            || self.entities.iter().any(|e| e == entity_id)
            || entity_id
                .split_once('.')
                .is_some_and(|(d, _)| self.domains.iter().any(|x| x == d))
    }

    /// The `listeners` of a `render_template` event. Iterating all states listens to everything,
    /// and HA then reports no entities or domains.
    pub fn json(&self) -> serde_json::Value {
        let (domains, entities) = if self.all {
            (vec![], vec![])
        } else {
            (self.domains.clone(), self.entities.clone())
        };
        json!({"all": self.all, "domains": domains, "entities": entities, "time": self.time})
    }
}

/// Render as HA does with `parse_result=False`: the stripped text.
pub fn render(
    states: &Arc<States>,
    tpl: &str,
    vars: &serde_json::Value,
    strict: bool,
) -> Result<(String, Listeners), String> {
    let hass = Arc::new(Hass {
        states: states.clone(),
        tpl: tpl.into(),
        seen: Mutex::default(),
    });
    let env = if strict { &*STRICT } else { &*LENIENT };
    let ctx = minijinja::context! { states => Value::from_dyn_object(hass.clone()), ..ser(vars) };
    let out = env.render_str(tpl, ctx).map_err(|e| ha_error(&e))?;
    let seen = hass.seen.lock().unwrap().clone();
    Ok((out.trim().to_string(), seen))
}

fn ser<T: serde::Serialize>(v: T) -> Value {
    minijinja::value::Serde(v).into()
}

/// `str(TemplateError(err))`: the Python exception type, then the message.
fn ha_error(e: &Error) -> String {
    let detail = e.detail().map_or_else(|| e.to_string(), String::from);
    match e.kind() {
        // Our helpers' errors already carry HA's text.
        ErrorKind::InvalidOperation if detail.starts_with("ValueError: ") => detail,
        ErrorKind::UndefinedError => format!("UndefinedError: {detail}"),
        ErrorKind::SyntaxError => format!("TemplateSyntaxError: {detail}"),
        _ => format!("TemplateError: {detail}"),
    }
}

/// HA's `_parse_result`: a rendered string back to a number, bool, None or container when it is
/// a Python literal, else the string itself.
pub fn parse_result(s: &str) -> serde_json::Value {
    if is_numeric(s) {
        let n = if s.contains('.') {
            s.parse::<f64>().ok().map(|f| json!(f))
        } else {
            s.parse::<i64>().ok().map(|i| json!(i))
        };
        if let Some(n) = n {
            return n;
        }
    }
    let mut p = Literal {
        s: s.as_bytes(),
        i: 0,
    };
    match p.top() {
        // Strings stay as rendered (quotes and all); numbers only when they look numeric
        // ("1e5", "0x10" stay strings).
        Some(v) if !v.is_string() && !(v.is_number() && !is_numeric(s)) => v,
        _ => s.into(),
    }
}

/// HA's `_IS_NUMERIC`: `^[+-]?(?!0\d)\d*(?:\.\d*)?$`
fn is_numeric(s: &str) -> bool {
    let b = s.strip_prefix(['+', '-']).unwrap_or(s).as_bytes();
    if b.len() > 1 && b[0] == b'0' && b[1].is_ascii_digit() {
        return false;
    }
    let int = b.iter().take_while(|c| c.is_ascii_digit()).count();
    match &b[int..] {
        [] => true,
        [b'.', rest @ ..] => rest.iter().all(u8::is_ascii_digit),
        _ => false,
    }
}

/// The subset of Python's `ast.literal_eval` a template result can be: numbers, strings, True,
/// False, None, lists, tuples, sets (as lists) and dicts with string keys.
struct Literal<'a> {
    s: &'a [u8],
    i: usize,
}

impl Literal<'_> {
    fn ws(&mut self) {
        while self.s.get(self.i).is_some_and(u8::is_ascii_whitespace) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.i) == Some(&c);
        self.i += hit as usize;
        hit
    }

    /// A whole result: one value, or a bare tuple (`1, 2`).
    fn top(&mut self) -> Option<serde_json::Value> {
        let first = self.value()?;
        let v = if self.eat(b',') {
            let mut items = vec![first];
            items.extend(self.items(None)?);
            items.into()
        } else {
            first
        };
        self.ws();
        (self.i == self.s.len()).then_some(v)
    }

    /// Comma-separated values up to `close` (or the end), trailing comma allowed.
    fn items(&mut self, close: Option<u8>) -> Option<Vec<serde_json::Value>> {
        let mut out = vec![];
        loop {
            self.ws();
            if close.map_or(self.i == self.s.len(), |c| self.eat(c)) {
                return Some(out);
            }
            out.push(self.value()?);
            if !self.eat(b',') {
                return close.is_none_or(|c| self.eat(c)).then_some(out);
            }
        }
    }

    fn value(&mut self) -> Option<serde_json::Value> {
        self.ws();
        let c = *self.s.get(self.i)?;
        match c {
            b'[' => {
                self.i += 1;
                Some(self.items(Some(b']'))?.into())
            }
            b'(' => {
                self.i += 1;
                Some(self.items(Some(b')'))?.into())
            }
            b'{' => {
                self.i += 1;
                let mut map = serde_json::Map::new();
                if self.eat(b'}') {
                    return Some(map.into());
                }
                let first = self.value()?;
                if !self.eat(b':') {
                    // a set
                    let mut items = vec![first];
                    if self.eat(b',') {
                        items.extend(self.items(Some(b'}'))?);
                    } else if !self.eat(b'}') {
                        return None;
                    }
                    return Some(items.into());
                }
                let mut key = first;
                loop {
                    let k = match key {
                        serde_json::Value::String(k) => k,
                        // shortcut: non-string dict keys become strings; JSON has no others.
                        k => k.to_string(),
                    };
                    map.insert(k, self.value()?);
                    if !self.eat(b',') || self.eat(b'}') {
                        break;
                    }
                    key = self.value()?;
                    if !self.eat(b':') {
                        return None;
                    }
                }
                self.ws();
                self.eat(b'}').then_some(map.into())
            }
            b'\'' | b'"' => self.string(c),
            _ => self.scalar(),
        }
    }

    fn string(&mut self, quote: u8) -> Option<serde_json::Value> {
        self.i += 1;
        let mut out = String::new();
        let rest = std::str::from_utf8(&self.s[self.i..]).ok()?;
        let mut chars = rest.char_indices();
        while let Some((n, c)) = chars.next() {
            match c {
                c if c as u32 == quote as u32 => {
                    self.i += n + 1;
                    return Some(out.into());
                }
                '\\' => match chars.next()?.1 {
                    'n' => out.push('\n'),
                    't' => out.push('\t'),
                    'r' => out.push('\r'),
                    '0' => out.push('\0'),
                    c @ ('\\' | '\'' | '"') => out.push(c),
                    // shortcut: \x, \u and \N escapes aren't decoded; repr() only emits them
                    // for non-printable characters.
                    _ => return None,
                },
                '\n' => return None,
                c => out.push(c),
            }
        }
        None
    }

    fn scalar(&mut self) -> Option<serde_json::Value> {
        let start = self.i;
        while self
            .s
            .get(self.i)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-' | b'_'))
        {
            // a sign only leads, or follows an exponent
            let c = self.s[self.i];
            if matches!(c, b'+' | b'-')
                && self.i != start
                && !matches!(self.s[self.i - 1], b'e' | b'E')
            {
                break;
            }
            self.i += 1;
        }
        let word = std::str::from_utf8(&self.s[start..self.i]).ok()?;
        match word {
            "True" => Some(true.into()),
            "False" => Some(false.into()),
            "None" => Some(serde_json::Value::Null),
            w if is_numeric(w) && !w.is_empty() && w != "." && w != "+" && w != "-" => {
                if w.contains('.') {
                    w.parse::<f64>().ok().map(Into::into)
                } else {
                    w.parse::<i64>().ok().map(Into::into)
                }
            }
            // Python literals is_numeric rejects (exponents, inf-free floats); kept as numbers so
            // parse_result can tell they aren't plain digits.
            w if w.contains(['e', 'E']) && !w.starts_with(['e', 'E']) => {
                w.parse::<f64>().ok().map(Into::into)
            }
            _ => None,
        }
    }
}

/// Python's `str()` of a template value, as Jinja's output: True/False/None, and containers
/// with their items' `repr()`.
fn py_str(v: &Value, out: &mut String) {
    match v.as_str() {
        Some(s) if v.kind() == minijinja::value::ValueKind::String => out.push_str(s),
        _ => py_repr(v, out),
    }
}

fn py_repr(v: &Value, out: &mut String) {
    use minijinja::value::ValueKind as K;
    match v.kind() {
        K::Undefined => {}
        K::None => out.push_str("None"),
        K::Bool => out.push_str(if v.is_true() { "True" } else { "False" }),
        K::Number => match (v.is_integer(), f64::try_from(v.clone())) {
            (false, Ok(f)) => py_float(f, out),
            _ => {
                let _ = write!(out, "{v}");
            }
        },
        K::String => {
            let s = v.as_str().unwrap_or_default();
            let q = if s.contains('\'') && !s.contains('"') {
                '"'
            } else {
                '\''
            };
            out.push(q);
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    c if c == q => {
                        out.push('\\');
                        out.push(c);
                    }
                    c => out.push(c),
                }
            }
            out.push(q);
        }
        K::Seq | K::Iterable => {
            out.push('[');
            for (i, item) in v.try_iter().into_iter().flatten().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(&item, out);
            }
            out.push(']');
        }
        K::Map if v.as_object().is_some_and(|o| o.repr() == ObjectRepr::Map) && !is_ours(v) => {
            out.push('{');
            for (i, k) in v.try_iter().into_iter().flatten().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                py_repr(&k, out);
                out.push_str(": ");
                py_repr(&v.get_item(&k).unwrap_or_default(), out);
            }
            out.push('}');
        }
        _ => {
            let _ = write!(out, "{v}");
        }
    }
}

fn is_ours(v: &Value) -> bool {
    v.downcast_object_ref::<TemplateState>().is_some() || v.downcast_object_ref::<Dt>().is_some()
}

/// Python's `repr(float)`: shortest round-trip digits, `.0` on whole numbers, exponent outside
/// 1e-4..1e16.
fn py_float(f: f64, out: &mut String) {
    if f.is_nan() {
        out.push_str("nan");
    } else if f.is_infinite() {
        out.push_str(if f > 0.0 { "inf" } else { "-inf" });
    } else if f != 0.0 && (f.abs() >= 1e16 || f.abs() < 1e-4) {
        let s = format!("{f:e}");
        let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
        let e: i32 = e.parse().unwrap_or(0);
        let _ = write!(out, "{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
    } else if f.fract() == 0.0 {
        let _ = write!(out, "{f:.1}");
    } else {
        let _ = write!(out, "{f}");
    }
}

/// The per-render `states` global: calls, `states.domain.object_id`, iteration.
#[derive(Debug)]
struct Hass {
    states: Arc<States>,
    /// For HA's "no default was specified" messages.
    tpl: String,
    seen: Mutex<Listeners>,
}

impl Hass {
    fn get(self: &Arc<Self>, entity_id: &str) -> Option<Arc<state::State>> {
        self.seen.lock().unwrap().entity(entity_id);
        self.states.get(entity_id)
    }

    fn no_default(&self, function: &str, value: &Value) -> Error {
        let mut v = String::new();
        py_str(value, &mut v);
        Error::new(
            ErrorKind::InvalidOperation,
            format!(
                "ValueError: Template error: {function} got invalid input '{v}' when rendering template '{}' but no default was specified",
                self.tpl
            ),
        )
    }

    fn template_state(self: &Arc<Self>, s: Arc<state::State>) -> Value {
        Value::from_object(TemplateState {
            hass: self.clone(),
            s,
        })
    }
}

impl Object for Hass {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    /// `states.light` (a domain) or `states['light.x']` (an entity).
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let name = key.as_str()?;
        if name.contains('.') {
            return Some(
                self.get(name)
                    .map_or(Value::from(()), |s| self.template_state(s)),
            );
        }
        Some(Value::from_object(Domain {
            hass: self.clone(),
            domain: name.into(),
        }))
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.seen.lock().unwrap().all = true;
        let items: Vec<Value> = self
            .states
            .all()
            .into_iter()
            .map(|s| self.template_state_quiet(s))
            .collect();
        Enumerator::Iter(Box::new(items.into_iter()))
    }

    /// `states('light.x', rounded, with_unit)`
    fn call(self: &Arc<Self>, _: &mut Vm<'_, '_>, args: &[Value]) -> Result<Value, Error> {
        let (entity_id, rounded, with_unit, kw): (&str, Option<bool>, Option<bool>, Kwargs) =
            minijinja::value::from_args(args)?;
        let rounded = rounded.or(kw.get("rounded")?);
        let with_unit = with_unit.or(kw.get("with_unit")?);
        kw.assert_all_used()?;
        Ok(states_fn(self, entity_id, rounded, with_unit).into())
    }

    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<template AllStates>")
    }
}

impl Hass {
    /// States from iteration don't add entity listeners (HA's `_template_state_no_collect`).
    fn template_state_quiet(self: &Arc<Self>, s: Arc<state::State>) -> Value {
        Value::from_object(TemplateState {
            hass: Arc::new(Hass {
                states: self.states.clone(),
                tpl: String::new(),
                seen: Mutex::default(),
            }),
            s,
        })
    }
}

fn states_fn(
    hass: &Arc<Hass>,
    entity_id: &str,
    _rounded: Option<bool>,
    with_unit: Option<bool>,
) -> String {
    let Some(s) = hass.get(entity_id) else {
        return "unknown".into();
    };
    // shortcut: `rounded` is ignored (HA rounds sensors to their display precision); add with
    // the sensor precision options.
    match s
        .attributes
        .get("unit_of_measurement")
        .and_then(|u| u.as_str())
    {
        Some(unit) if with_unit == Some(true) => format!("{} {unit}", s.state),
        _ => s.state.clone(),
    }
}

#[derive(Debug)]
struct Domain {
    hass: Arc<Hass>,
    domain: String,
}

impl Object for Domain {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Iterable
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let id = format!("{}.{}", self.domain, key.as_str()?);
        Some(
            self.hass
                .get(&id)
                .map_or(Value::from(()), |s| self.hass.template_state(s)),
        )
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        self.hass.seen.lock().unwrap().domain(&self.domain);
        let prefix = format!("{}.", self.domain);
        let items: Vec<Value> = self
            .hass
            .states
            .all()
            .into_iter()
            .filter(|s| s.entity_id.starts_with(&prefix))
            .map(|s| self.hass.template_state_quiet(s))
            .collect();
        Enumerator::Iter(Box::new(items.into_iter()))
    }

    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<template DomainStates('{}')>", self.domain)
    }
}

/// HA's `TemplateState`: a state object in a template.
#[derive(Debug)]
struct TemplateState {
    hass: Arc<Hass>,
    s: Arc<state::State>,
}

impl Object for TemplateState {
    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        let s = &self.s;
        let key = key.as_str()?;
        let (domain, object_id) = s.entity_id.split_once('.').unwrap_or(("", &s.entity_id));
        if key == "entity_id" {
            return Some(Value::from(&*s.entity_id));
        }
        self.hass.seen.lock().unwrap().entity(&s.entity_id);
        Some(match key {
            "state" => Value::from(s.state.as_str()),
            "attributes" => ser(&s.attributes),
            "last_changed" => Value::from_object(Dt::utc(s.last_changed)),
            "last_updated" | "last_reported" => Value::from_object(Dt::utc(s.last_updated)),
            "context" => ser(s.context.json()),
            "domain" => Value::from(domain),
            "object_id" => Value::from(object_id),
            // `State.name`: the friendly name, else the object id with spaces.
            "name" => Value::from(
                s.attributes
                    .get("friendly_name")
                    .and_then(|n| n.as_str())
                    .map_or_else(|| object_id.replace('_', " "), String::from),
            ),
            "state_with_unit" => {
                Value::from(states_fn(&self.hass, &s.entity_id, Some(true), Some(true)))
            }
            _ => return None,
        })
    }

    fn enumerate(self: &Arc<Self>) -> Enumerator {
        Enumerator::Str(&[
            "entity_id",
            "state",
            "attributes",
            "last_changed",
            "last_updated",
            "context",
            "domain",
            "object_id",
            "name",
        ])
    }

    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // shortcut: HA's repr also lists the attributes and last_changed; nobody parses it.
        write!(
            f,
            "<template TemplateState(<state {}={}>)>",
            self.s.entity_id, self.s.state
        )
    }
}

/// A time-zone-aware Python `datetime`: UTC for state times, local for `now()`, as in HA.
#[derive(Debug)]
struct Dt(DateTime<FixedOffset>);

impl Dt {
    fn utc(t: DateTime<Utc>) -> Self {
        Self(t.fixed_offset())
    }

    /// Python's `isoformat()` / `str()`: microseconds only when non-zero.
    fn python(&self, sep: char) -> String {
        let micros = if self.0.timestamp_subsec_micros() == 0 {
            ""
        } else {
            "%.6f"
        };
        let fmt = format!("%Y-%m-%d{sep}%H:%M:%S{micros}%:z");
        self.0.format(&fmt).to_string()
    }
}

impl Object for Dt {
    fn repr(self: &Arc<Self>) -> ObjectRepr {
        ObjectRepr::Plain
    }

    fn get_value(self: &Arc<Self>, key: &Value) -> Option<Value> {
        use chrono::{Datelike, Timelike};
        let t = self.0;
        Some(Value::from(match key.as_str()? {
            "year" => t.year() as i64,
            "month" => t.month() as i64,
            "day" => t.day() as i64,
            "hour" => t.hour() as i64,
            "minute" => t.minute() as i64,
            "second" => t.second() as i64,
            "microsecond" => (t.nanosecond() / 1000) as i64,
            _ => return None,
        }))
    }

    fn call_method(
        self: &Arc<Self>,
        _: &mut Vm<'_, '_>,
        method: &str,
        args: &[Value],
    ) -> Result<Value, Error> {
        use chrono::Datelike;
        match method {
            "isoformat" => Ok(self.python('T').into()),
            "timestamp" => Ok(timestamp(self.0.with_timezone(&Utc)).into()),
            "weekday" => Ok((self.0.weekday().num_days_from_monday() as i64).into()),
            "strftime" => {
                let (fmt,): (&str,) = minijinja::value::from_args(args)?;
                let mut s = String::new();
                write!(s, "{}", self.0.format(fmt)).map_err(|_| {
                    Error::new(ErrorKind::InvalidOperation, "invalid strftime format")
                })?;
                Ok(s.into())
            }
            _ => Err(Error::from(ErrorKind::UnknownMethod)),
        }
    }

    fn custom_cmp(
        self: &Arc<Self>,
        other: &minijinja::value::DynObject,
    ) -> Option<std::cmp::Ordering> {
        other.downcast_ref::<Dt>().map(|o| self.0.cmp(&o.0))
    }

    /// `str(datetime)`
    fn render(self: &Arc<Self>, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.python(' '))
    }
}

fn timestamp(t: DateTime<Utc>) -> f64 {
    t.timestamp_micros() as f64 / 1e6
}

static LENIENT: LazyLock<Environment<'static>> = LazyLock::new(|| env(false));
static STRICT: LazyLock<Environment<'static>> = LazyLock::new(|| env(true));

fn hass(vm: &Vm) -> Result<Arc<Hass>, Error> {
    vm.lookup("states")
        .and_then(|v| v.downcast_object::<Hass>())
        .ok_or_else(|| Error::new(ErrorKind::InvalidOperation, "states is not available"))
}

/// A value as a string the way Python's `str()` would make it, for parsing.
fn text(v: &Value) -> String {
    let mut s = String::new();
    py_str(v, &mut s);
    s
}

/// Python's `float(value)`: numbers, and strings with surrounding whitespace.
fn to_float(v: &Value) -> Option<f64> {
    use minijinja::value::ValueKind as K;
    match v.kind() {
        K::Number => f64::try_from(v.clone()).ok(),
        K::Bool => Some(v.is_true() as i64 as f64),
        K::String => {
            let s = v.as_str()?.trim().replace('_', "");
            match s.to_ascii_lowercase().as_str() {
                "inf" | "+inf" | "infinity" | "+infinity" => Some(f64::INFINITY),
                "-inf" | "-infinity" => Some(f64::NEG_INFINITY),
                "nan" | "+nan" | "-nan" => Some(f64::NAN),
                _ => s.parse().ok(),
            }
        }
        _ => None,
    }
}

/// Jinja's `do_int`: ints, floats truncated, strings as ints in `base` or as floats.
fn to_int(v: &Value, base: u32) -> Option<i64> {
    use minijinja::value::ValueKind as K;
    match v.kind() {
        K::Number if v.is_integer() => v.as_i64(),
        K::Number => f64::try_from(v.clone())
            .ok()
            .filter(|f| f.is_finite())
            .map(|f| f as i64),
        K::Bool => Some(v.is_true() as i64),
        K::String => {
            let s = v.as_str()?.trim();
            let digits = match base {
                16 => s.trim_start_matches("0x").trim_start_matches("0X"),
                8 => s.trim_start_matches("0o").trim_start_matches("0O"),
                2 => s.trim_start_matches("0b").trim_start_matches("0B"),
                _ => s,
            };
            i64::from_str_radix(&digits.replace('_', ""), base)
                .ok()
                .or_else(|| {
                    (base == 10)
                        .then(|| s.parse::<f64>().ok())
                        .flatten()
                        .filter(|f| f.is_finite())
                        .map(|f| f as i64)
                })
        }
        _ => None,
    }
}

/// `cv.boolean`
fn to_bool(v: &Value) -> Option<bool> {
    use minijinja::value::ValueKind as K;
    match v.kind() {
        K::Bool => Some(v.is_true()),
        K::Number => to_float(v).map(|f| f != 0.0),
        K::String => match v.as_str()?.trim().to_lowercase().as_str() {
            "1" | "true" | "yes" | "on" | "enable" => Some(true),
            "0" | "false" | "no" | "off" | "disable" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn or_default(
    vm: &Vm,
    name: &str,
    value: &Value,
    converted: Option<Value>,
    default: Option<Value>,
) -> Result<Value, Error> {
    match (converted, default) {
        (Some(v), _) => Ok(v),
        (None, Some(d)) => Ok(d),
        (None, None) => Err(hass(vm)?.no_default(name, value)),
    }
}

fn parse_datetime(s: &str) -> Option<DateTime<Utc>> {
    let s = s.trim();
    let iso = s.replacen(' ', "T", 1);
    if let Ok(t) = DateTime::parse_from_rfc3339(&iso) {
        return Some(t.with_timezone(&Utc));
    }
    // Naive times are in the configured time zone.
    [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%dT%H:%M",
    ]
    .iter()
    .find_map(|f| chrono::NaiveDateTime::parse_from_str(&iso, f).ok())
    .or_else(|| {
        chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .ok()
            .and_then(|d| d.and_hms_opt(0, 0, 0))
    })
    .map(|n| local_tz().local_to_utc(n))
}

fn env(strict: bool) -> Environment<'static> {
    let mut env = Environment::new();
    env.set_undefined_behavior(if strict {
        UndefinedBehavior::Strict
    } else {
        UndefinedBehavior::Lenient
    });
    env.set_formatter(|out, _, v| {
        let mut s = String::new();
        py_str(v, &mut s);
        out.write_str(&s).map_err(Error::from)
    });

    fn states_filter(vm: &Vm, entity_id: &str, kw: Rest<Value>) -> Result<String, Error> {
        let _ = kw;
        Ok(states_fn(&hass(vm)?, entity_id, None, None))
    }
    env.add_filter("states", states_filter);

    fn is_state(vm: &Vm, entity_id: &str, state: Value) -> Result<bool, Error> {
        let Some(s) = hass(vm)?.get(entity_id) else {
            return Ok(false);
        };
        Ok(state.as_str() == Some(s.state.as_str())
            || (state.kind() == minijinja::value::ValueKind::Seq
                && state
                    .try_iter()?
                    .any(|v| v.as_str() == Some(s.state.as_str()))))
    }
    env.add_function("is_state", is_state);
    env.add_test("is_state", is_state);

    fn state_attr(vm: &Vm, entity_id: &str, name: &str) -> Result<Value, Error> {
        Ok(hass(vm)?
            .get(entity_id)
            .and_then(|s| s.attributes.get(name).map(ser))
            .unwrap_or(Value::from(())))
    }
    env.add_function("state_attr", state_attr);
    env.add_filter("state_attr", state_attr);

    fn is_state_attr(vm: &Vm, entity_id: &str, name: &str, value: Value) -> Result<bool, Error> {
        Ok(hass(vm)?
            .get(entity_id)
            .and_then(|s| s.attributes.get(name).map(ser))
            .is_some_and(|a| a == value))
    }
    env.add_function("is_state_attr", is_state_attr);
    env.add_test("is_state_attr", is_state_attr);

    fn has_value(vm: &Vm, entity_id: &str) -> Result<bool, Error> {
        Ok(hass(vm)?
            .get(entity_id)
            .is_some_and(|s| s.state != "unavailable" && s.state != "unknown"))
    }
    env.add_function("has_value", has_value);
    env.add_filter("has_value", has_value);
    env.add_test("has_value", has_value);

    fn float(vm: &Vm, v: Value, default: Option<Value>) -> Result<Value, Error> {
        let f = to_float(&v).map(Value::from);
        or_default(vm, "float", &v, f, default)
    }
    env.add_function("float", float);
    env.add_filter("float", float);

    fn int(vm: &Vm, v: Value, default: Option<Value>, base: Option<u32>) -> Result<Value, Error> {
        let i = to_int(&v, base.unwrap_or(10)).map(Value::from);
        or_default(vm, "int", &v, i, default)
    }
    env.add_function("int", int);
    env.add_filter("int", int);

    fn bool_(vm: &Vm, v: Value, default: Option<Value>) -> Result<Value, Error> {
        let b = to_bool(&v).map(Value::from);
        or_default(vm, "bool", &v, b, default)
    }
    env.add_function("bool", bool_);
    env.add_filter("bool", bool_);

    fn is_number(v: Value) -> bool {
        v.kind() != minijinja::value::ValueKind::Bool && to_float(&v).is_some_and(f64::is_finite)
            || v.kind() == minijinja::value::ValueKind::Bool
    }
    env.add_function("is_number", is_number);
    env.add_filter("is_number", is_number);
    env.add_test("is_number", is_number);

    fn iif(
        v: Value,
        if_true: Option<Value>,
        if_false: Option<Value>,
        if_none: Option<Value>,
    ) -> Value {
        match if_none {
            Some(n) if v.is_none() => n,
            _ if v.is_true() => if_true.unwrap_or(Value::from(true)),
            _ => if_false.unwrap_or(Value::from(false)),
        }
    }
    env.add_function("iif", iif);
    env.add_filter("iif", iif);

    env.add_filter("is_defined", |v: Value| {
        if v.is_undefined() {
            Err(Error::new(ErrorKind::UndefinedError, "value is undefined"))
        } else {
            Ok(v)
        }
    });

    /// HA's `forgiving_round`: an int with precision 0, else a float.
    fn round(
        vm: &Vm,
        v: Value,
        precision: Option<i32>,
        method: Option<&str>,
        kw: Kwargs,
    ) -> Result<Value, Error> {
        let default: Option<Value> = kw.get("default")?;
        let p = precision.unwrap_or(0);
        let m = 10f64.powi(p);
        let r = to_float(&v)
            .filter(|f| f.is_finite())
            .map(|f| match method.unwrap_or("common") {
                "ceil" => (f * m).ceil() / m,
                "floor" => (f * m).floor() / m,
                "half" => (f * 2.0).round_ties_even() / 2.0,
                // Python's round(): banker's rounding on the decimal value.
                _ => (f * m).round_ties_even() / m,
            });
        let r = r.map(|r| {
            if p == 0 {
                Value::from(r as i64)
            } else {
                Value::from(r)
            }
        });
        or_default(vm, "round", &v, r, default)
    }
    env.add_filter("round", round);

    env.add_function("now", |vm: &Vm| -> Result<Value, Error> {
        hass(vm)?.seen.lock().unwrap().time = true;
        let now = Utc::now();
        Ok(Value::from_object(Dt(
            now.with_timezone(&local_tz().offset(now))
        )))
    });
    env.add_function("utcnow", |vm: &Vm| -> Result<Value, Error> {
        hass(vm)?.seen.lock().unwrap().time = true;
        Ok(Value::from_object(Dt::utc(Utc::now())))
    });

    fn as_timestamp(vm: &Vm, v: Value, default: Option<Value>) -> Result<Value, Error> {
        let t = match v.downcast_object_ref::<Dt>() {
            Some(d) => Some(d.0.with_timezone(&Utc)),
            None => parse_datetime(&text(&v)),
        };
        or_default(
            vm,
            "as_timestamp",
            &v,
            t.map(|t| Value::from(timestamp(t))),
            default,
        )
    }
    env.add_function("as_timestamp", as_timestamp);
    env.add_filter("as_timestamp", as_timestamp);

    fn to_json(v: Value, kw: Kwargs) -> Result<String, Error> {
        let pretty: Option<bool> = kw.get("pretty_print")?;
        let sort: Option<bool> = kw.get("sort_keys")?;
        let _: Option<bool> = kw.get("ensure_ascii")?;
        kw.assert_all_used()?;
        let mut j = serde_json::to_value(&v)
            .map_err(|e| Error::new(ErrorKind::BadSerialization, e.to_string()))?;
        if sort == Some(true) {
            j.sort_all_objects();
        }
        // orjson's compact separators, or indent 2 with pretty_print
        Ok(if pretty == Some(true) {
            serde_json::to_string_pretty(&j)
        } else {
            serde_json::to_string(&j)
        }
        .unwrap_or_default())
    }
    env.add_filter("to_json", to_json);

    fn from_json(vm: &Vm, v: Value, default: Option<Value>) -> Result<Value, Error> {
        let parsed = serde_json::from_str::<serde_json::Value>(&text(&v))
            .ok()
            .map(|j| ser(&j));
        or_default(vm, "from_json", &v, parsed, default)
    }
    env.add_filter("from_json", from_json);
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::Context;

    fn states() -> Arc<States> {
        let s = Arc::new(States::default());
        let attrs = |v: serde_json::Value| v.as_object().unwrap().clone();
        s.set(
            "sensor.temp",
            "21.5",
            attrs(json!({"unit_of_measurement": "°C", "friendly_name": "Temperature"})),
            Context::new(None),
        );
        s.set(
            "light.ceiling",
            "on",
            attrs(json!({"brightness": 76})),
            Context::new(None),
        );
        s.set(
            "light.desk",
            "off",
            attrs(json!({"brightness": null})),
            Context::new(None),
        );
        s.set(
            "binary_sensor.door",
            "off",
            attrs(json!({})),
            Context::new(None),
        );
        s
    }

    fn r(tpl: &str) -> String {
        render(&states(), tpl, &json!({}), false).unwrap().0
    }

    #[test]
    fn recorded_template() {
        // The template in compat/fixtures/automations, with its first recorded result.
        let s = states();
        s.set(
            "light.ceiling",
            "off",
            Default::default(),
            Context::new(None),
        );
        let tpl = "{{ states('sensor.temp') }}|{{ is_state('binary_sensor.door', 'on') }}|{{ state_attr('light.ceiling', 'brightness') }}|{{ states('sensor.missing') | float(0) }}|{{ iif(is_state('switch.fan', 'on'), 'fan', 'still') }}";
        let (out, l) = render(&s, tpl, &json!({}), false).unwrap();
        assert_eq!(out, "21.5|False|None|0|still");
        assert_eq!(
            l.json(),
            json!({"all": false, "domains": [], "entities": ["sensor.temp", "binary_sensor.door", "light.ceiling", "sensor.missing", "switch.fan"], "time": false})
        );
        assert!(l.matches("switch.fan") && !l.matches("light.desk"));
    }

    /// The live template in compat/fixtures/automations: the device changes HA saw, in order,
    /// must give the recorded results (a re-render with an unchanged result sends nothing).
    #[test]
    fn recorded_sequence() {
        let s = Arc::new(States::default());
        let set = |id: &str, st: &str, a: serde_json::Value| {
            s.set(id, st, a.as_object().unwrap().clone(), Context::new(None))
        };
        let t = "sensor.demo_device_temperature";
        let door = "binary_sensor.demo_device_front_door";
        let (ceiling, fan) = ("light.demo_device_ceiling", "switch.demo_device_fan");
        set(t, "21.5", json!({}));
        set(door, "off", json!({}));
        set(ceiling, "off", json!({"brightness": null}));
        set(fan, "off", json!({}));
        let tpl = "{{ states('sensor.demo_device_temperature') }}|{{ is_state('binary_sensor.demo_device_front_door', 'on') }}|{{ state_attr('light.demo_device_ceiling', 'brightness') }}|{{ states('sensor.missing') | float(0) }}|{{ iif(is_state('switch.demo_device_fan', 'on'), 'fan', 'still') }}";
        let mut got: Vec<String> = vec![];
        let mut render_if_changed = || {
            let out = render(&s, tpl, &json!({}), false).unwrap().0;
            if got.last() != Some(&out) {
                got.push(out);
            }
        };
        render_if_changed();
        for (id, st, a) in [
            (fan, "on", json!({})),
            (ceiling, "on", json!({"brightness": 76})),
            (fan, "off", json!({})),
            (door, "on", json!({})),
            (ceiling, "on", json!({"brightness": 76})), // the script turns it on again: no change
            (fan, "on", json!({})),
            (t, "22.0", json!({})),
        ] {
            set(id, st, a);
            render_if_changed();
        }
        let want = include_str!("../../../compat/fixtures/automations/ws.jsonl")
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|l| l["dir"] == "recv")
            .flat_map(|l| match &l["msg"] {
                serde_json::Value::Array(a) => a.clone(),
                m => vec![m.clone()],
            })
            .filter(|m| m["id"] == 50 && m["event"]["result"].is_string())
            .map(|m| m["event"]["result"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(want.len(), 7);
        assert_eq!(got, want);
    }

    #[test]
    fn helpers_match_ha() {
        assert_eq!(r("{{ states('sensor.temp', with_unit=True) }}"), "21.5 °C");
        assert_eq!(
            r("{{ states.sensor.temp.state }} {{ states.sensor.temp.name }}"),
            "21.5 Temperature"
        );
        assert_eq!(r("{{ states.light.nope }}"), "None");
        assert_eq!(r("{{ 'light.ceiling' | states }}"), "on");
        assert_eq!(r("{{ is_state('light.ceiling', ['on', 'off']) }}"), "True");
        assert_eq!(r("{{ 'light.ceiling' is is_state('on') }}"), "True");
        assert_eq!(
            r("{{ is_state_attr('light.ceiling', 'brightness', 76) }}"),
            "True"
        );
        assert_eq!(
            r("{{ has_value('light.ceiling') }} {{ has_value('light.nope') }}"),
            "True False"
        );
        assert_eq!(
            r("{{ '3.5' | float + 1 }} {{ float('x', 2) }} {{ 'x' | float(0) }}"),
            "4.5 2 0"
        );
        assert_eq!(
            r("{{ '42' | int }} {{ 3.9 | int }} {{ 'ff' | int(0, 16) }} {{ 'x' | int(-1) }}"),
            "42 3 255 -1"
        );
        assert_eq!(
            r("{{ 'on' | bool }} {{ bool('nah', 'dunno') }}"),
            "True dunno"
        );
        assert_eq!(
            r("{{ 2.5 | round }} {{ 3.14159 | round(2) }} {{ 2.4 | round(0, 'ceil') }}"),
            "2 3.14 3"
        );
        assert_eq!(
            r("{{ iif(none, 'y', 'n', 'none') }} {{ (1 == 1) | iif('yes', 'no') }}"),
            "none yes"
        );
        assert_eq!(
            r("{{ is_number('1.5') }} {{ '1.5' is is_number }} {{ is_number('x') }}"),
            "True True False"
        );
        assert_eq!(
            r("{{ {'a': [1, 'b', none, true]} | to_json }}"),
            r#"{"a":[1,"b",null,true]}"#
        );
        assert_eq!(r("{{ ('{\"x\": 1}' | from_json).x }}"), "1");
        assert_eq!(
            r("{{ as_timestamp('2026-11-24T00:00:00+00:00') }}"),
            "1795478400.0"
        );
        assert_eq!(
            r("{% for s in states.light %}{{ s.entity_id }} {% endfor %}"),
            "light.ceiling light.desk"
        );
        assert_eq!(
            r(
                "{% for s in states.light %}{% if loop.index == 1 %}{% break %}{% endif %}{% endfor %}x"
            ),
            "x"
        );
    }

    #[test]
    fn local_time_zone() {
        set_time_zone(Zone::load("Europe/Amsterdam").unwrap());
        // Naive times are local; aware ones keep their offset.
        let naive = r("{{ as_timestamp('2026-07-01 12:00:00') }}");
        let now = r("{{ now().isoformat() }} {{ utcnow().isoformat() }}");
        set_time_zone(Zone::utc());
        assert_eq!(naive, "1782900000.0"); // 10:00 UTC (CEST, +02:00)
        let (local, utc) = now.split_once(' ').unwrap();
        assert!(
            local.ends_with("+01:00") || local.ends_with("+02:00"),
            "{local}"
        );
        assert!(utc.ends_with("+00:00"), "{utc}");
        assert_eq!(
            r("{{ states.sensor.temp.last_changed.isoformat()[-6:] }}"),
            "+00:00"
        );
    }

    #[test]
    fn no_default_errors_like_ha() {
        let e = render(&states(), "{{ 'x' | float }}", &json!({}), false).unwrap_err();
        assert_eq!(
            e,
            "ValueError: Template error: float got invalid input 'x' when rendering template '{{ 'x' | float }}' but no default was specified"
        );
        assert!(render(&states(), "{{ nope | is_defined }}", &json!({}), false).is_err());
        assert!(render(&states(), "{{ nope }}", &json!({}), true).is_err());
        assert_eq!(
            render(&states(), "{{ nope }}", &json!({}), false)
                .unwrap()
                .0,
            ""
        );
    }

    #[test]
    fn listeners() {
        let s = states();
        let l = |t| render(&s, t, &json!({}), false).unwrap().1;
        assert_eq!(l("{{ states.light | list | count }}").domains, ["light"]);
        let all = l("{{ states | count }}");
        assert!(all.all && all.matches("anything.at_all"));
        assert_eq!(all.json()["entities"], json!([]));
        assert!(l("{{ now().year > 2000 }}").time);
        // entity_id doesn't subscribe; other attributes do
        assert!(
            l("{% for s in states.light %}{{ s.entity_id }}{% endfor %}")
                .entities
                .is_empty()
        );
        assert_eq!(
            l("{{ states.light.desk.attributes.brightness }}").entities,
            ["light.desk"]
        );
    }

    #[test]
    fn python_output() {
        assert_eq!(
            r("{{ [1, 'a', none, true, 1.0, 1e20] }}"),
            "[1, 'a', None, True, 1.0, 1e+20]"
        );
        assert_eq!(r("{{ {'k': \"it's\"} }}"), r#"{'k': "it's"}"#);
        assert_eq!(
            r("{{ 7 / 2 }} {{ 4 / 2 }} {{ 0.1 + 0.2 }}"),
            "3.5 2.0 0.30000000000000004"
        );
        assert_eq!(r("{{ true }}{{ none }}"), "TrueNone");
    }

    #[test]
    fn parse_result_like_ha() {
        let p = parse_result;
        assert_eq!(p("21.5"), json!(21.5));
        assert_eq!(p("42"), json!(42));
        assert_eq!(p("-3"), json!(-3));
        assert_eq!(p("01"), json!("01"));
        assert_eq!(p("1e5"), json!("1e5"));
        assert_eq!(p("True"), json!(true));
        assert_eq!(p("None"), json!(null));
        assert_eq!(p("on"), json!("on"));
        assert_eq!(p("'quoted'"), json!("'quoted'"));
        assert_eq!(
            p("21.5|False|None|0|still"),
            json!("21.5|False|None|0|still")
        );
        assert_eq!(p("[1, 'a', None, True]"), json!([1, "a", null, true]));
        assert_eq!(
            p("{'a': [1, 2], 'b': {'c': 'd'}}"),
            json!({"a": [1, 2], "b": {"c": "d"}})
        );
        assert_eq!(p("1, 2"), json!([1, 2]));
        assert_eq!(p("(1,)"), json!([1]));
        assert_eq!(p("[1, 2"), json!("[1, 2"));
        assert_eq!(p(""), json!(""));
        assert_eq!(p("## Welcome Dev"), json!("## Welcome Dev"));
    }
}
