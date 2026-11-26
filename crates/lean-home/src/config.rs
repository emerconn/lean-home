//! `configuration.yaml` as HA loads it (`annotatedyaml` on PyYAML's SafeLoader): YAML 1.1
//! scalars, anchors and merge keys, HA's tags (`!include`, `!include_dir_*`, `!secret`,
//! `!env_var`), then the core config (`.storage/core.config`, overridden by `homeassistant:`)
//! and the automation and script configs.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use saphyr_parser::{Event, Parser, ScalarStyle, Tag};
use serde_json::{Map, Value, json};

/// The parts of the config lean-home uses.
#[derive(Debug, Default)]
pub struct Config {
    pub core: Core,
    /// Every `automation:` / `automation <label>:` entry, in file order; validated in step 2.4.
    pub automations: Vec<Value>,
    /// `script:` / `script <label>:`, merged by script id.
    pub scripts: Map<String, Value>,
}

/// HA's `Config` location and units (`core_config.py`), with its defaults.
#[derive(Debug, Clone, PartialEq)]
pub struct Core {
    pub location_name: Value,
    pub latitude: Value,
    pub longitude: Value,
    pub elevation: Value,
    pub radius: Value,
    pub time_zone: String,
    pub tz: Zone,
    /// `metric` or `us_customary`
    pub unit_system: String,
    pub currency: Value,
    pub country: Value,
    pub language: Value,
    pub internal_url: Value,
    pub external_url: Value,
    /// `default`, `storage` (onboarding / the UI) or `yaml`
    pub config_source: &'static str,
}

impl Default for Core {
    fn default() -> Self {
        Self {
            location_name: json!("Home"),
            latitude: json!(0),
            longitude: json!(0),
            elevation: json!(0),
            radius: json!(100),
            time_zone: "UTC".into(),
            tz: Zone::utc(),
            unit_system: "metric".into(),
            currency: json!("EUR"),
            country: Value::Null,
            language: json!("en"),
            internal_url: Value::Null,
            external_url: Value::Null,
            config_source: "default",
        }
    }
}

impl Core {
    /// `UnitSystem.as_dict()`
    pub fn units_json(&self) -> Value {
        if self.unit_system == "us_customary" {
            json!({"length": "mi", "accumulated_precipitation": "in", "area": "ft²", "mass": "lb", "pressure": "psi", "temperature": "°F", "volume": "gal", "wind_speed": "mph"})
        } else {
            json!({"length": "km", "accumulated_precipitation": "mm", "area": "m²", "mass": "g", "pressure": "Pa", "temperature": "°C", "volume": "L", "wind_speed": "m/s"})
        }
    }

    /// Storage first, then any `homeassistant:` key from YAML on top (HA's
    /// `async_process_ha_core_config`).
    fn load(
        storage: Option<&Map<String, Value>>,
        yaml: &Map<String, Value>,
    ) -> Result<Self, String> {
        let mut core = Self::default();
        let mut apply = |src: &Map<String, Value>, unit_key: &str| -> Result<(), String> {
            for (key, slot) in [
                ("latitude", &mut core.latitude),
                ("longitude", &mut core.longitude),
                ("elevation", &mut core.elevation),
                ("radius", &mut core.radius),
                ("currency", &mut core.currency),
                ("language", &mut core.language),
            ] {
                if let Some(v) = src.get(key).filter(|v| !v.is_null()) {
                    *slot = v.clone();
                }
            }
            // Storage names it location_name, YAML name; urls and country may be set to null.
            if let Some(v) = src
                .get("location_name")
                .or(src.get("name"))
                .filter(|v| !v.is_null())
            {
                core.location_name = v.clone();
            }
            for (key, slot) in [
                ("country", &mut core.country),
                ("internal_url", &mut core.internal_url),
                ("external_url", &mut core.external_url),
            ] {
                if let Some(v) = src.get(key) {
                    *slot = v.clone();
                }
            }
            if let Some(tz) = src.get("time_zone").and_then(Value::as_str) {
                core.tz = Zone::load(tz)?;
                core.time_zone = tz.into();
            }
            if let Some(u) = src.get(unit_key).and_then(Value::as_str) {
                core.unit_system = match u {
                    // "imperial" is HA's old name; it raises a repair issue and uses us_customary.
                    "us_customary" | "imperial" => "us_customary".into(),
                    // HA falls back to metric for unknown names in storage.
                    _ => "metric".into(),
                };
            }
            Ok(())
        };
        if let Some(s) = storage {
            apply(s, "unit_system_v2")?;
            core.config_source = "storage";
        }
        apply(yaml, "unit_system")?;
        const YAML_KEYS: [&str; 12] = [
            "country",
            "currency",
            "elevation",
            "external_url",
            "internal_url",
            "language",
            "latitude",
            "longitude",
            "name",
            "radius",
            "time_zone",
            "unit_system",
        ];
        if YAML_KEYS.iter().any(|k| yaml.contains_key(*k)) {
            core.config_source = "yaml";
        }
        Ok(core)
    }
}

/// A time zone from the system's zoneinfo, as HA gets it from Python's `zoneinfo`: only the
/// configured zone is read, instead of embedding the whole database (+2 MB RSS with chrono-tz).
#[derive(Clone, Debug)]
pub struct Zone {
    name: String,
    /// None is UTC, which needs no file.
    tz: Option<std::sync::Arc<tz::TimeZone>>,
}

impl PartialEq for Zone {
    fn eq(&self, other: &Self) -> bool {
        self.name == other.name
    }
}

/// Python's default `zoneinfo.TZPATH`; `$TZDIR` first, as glibc does.
const TZPATH: [&str; 4] = [
    "/usr/share/zoneinfo",
    "/usr/lib/zoneinfo",
    "/usr/share/lib/zoneinfo",
    "/etc/zoneinfo",
];

impl Zone {
    pub fn utc() -> Self {
        Self {
            name: "UTC".into(),
            tz: None,
        }
    }

    pub fn load(name: &str) -> Result<Self, String> {
        if name == "UTC" || name == "Etc/UTC" {
            return Ok(Self {
                name: name.into(),
                tz: None,
            });
        }
        let invalid = || format!("invalid time zone {name:?}");
        // A zone name, not a path: no `..`, no leading `/`.
        if name.is_empty()
            || name.starts_with('/')
            || name.split('/').any(|p| p.is_empty() || p == "..")
        {
            return Err(invalid());
        }
        let dirs = std::env::var("TZDIR")
            .into_iter()
            .chain(TZPATH.map(String::from));
        for dir in dirs {
            if let Ok(bytes) = std::fs::read(Path::new(&dir).join(name)) {
                let tz = tz::TimeZone::from_tz_data(&bytes).map_err(|_| invalid())?;
                return Ok(Self {
                    name: name.into(),
                    tz: Some(tz.into()),
                });
            }
        }
        Err(format!(
            "{}: not found in $TZDIR or {}",
            invalid(),
            TZPATH.join(", ")
        ))
    }

    /// The UTC offset in effect at `t`.
    pub fn offset(&self, t: chrono::DateTime<chrono::Utc>) -> chrono::FixedOffset {
        let secs = self
            .tz
            .as_ref()
            .and_then(|tz| tz.find_local_time_type(t.timestamp()).ok())
            .map_or(0, |l| l.ut_offset());
        chrono::FixedOffset::east_opt(secs).unwrap_or(chrono::FixedOffset::east_opt(0).unwrap())
    }

    /// A wall-clock time in this zone, as UTC, like Python's `fold=0`: in a DST overlap the
    /// first occurrence, in a DST gap the offset from before the gap.
    pub fn local_to_utc(&self, n: chrono::NaiveDateTime) -> chrono::DateTime<chrono::Utc> {
        let wall = n.and_utc();
        let day = chrono::Duration::days(1);
        let before = self.offset(wall - day);
        let after = self.offset(wall + day);
        [before, after]
            .into_iter()
            .map(|o| wall - o)
            // consistent: the offset in effect at that instant is the one we assumed
            .find(|u| wall - self.offset(*u) == *u)
            .unwrap_or(wall - before)
    }
}

/// Load `configuration.yaml` (absent means an empty config) and `.storage/core.config`.
pub fn load(config_dir: &Path) -> Result<Config, String> {
    let secrets = Secrets::new(config_dir);
    let path = config_dir.join("configuration.yaml");
    let root = match load_yaml(&path, Some(&secrets)) {
        Ok(Value::Object(m)) => m,
        Ok(Value::Null) => Map::new(),
        Ok(_) => return Err(format!("{}: does not contain a dict", path.display())),
        Err(e) if e.not_found => Map::new(),
        Err(e) => return Err(e.msg),
    };
    let ha = match root.get("homeassistant") {
        Some(Value::Object(m)) => m.clone(),
        Some(Value::Null) | None => Map::new(),
        Some(_) => return Err("homeassistant: expected a dictionary".into()),
    };
    if ha.contains_key("packages") {
        // shortcut: packages aren't merged yet; automations and scripts in them are missing.
        eprintln!("config: homeassistant.packages is not supported yet; its contents are ignored");
    }
    let storage = std::fs::read_to_string(config_dir.join(".storage/core.config"))
        .ok()
        .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        .and_then(|v| v.get("data").and_then(Value::as_object).cloned());
    let core = Core::load(storage.as_ref(), &ha)?;

    let mut config = Config {
        core,
        ..Default::default()
    };
    for (key, value) in &root {
        if domain_key(key, "automation") {
            // A single automation or a list of them (HA's `extract_domain_configs`).
            match value {
                Value::Array(a) => config.automations.extend(a.iter().cloned()),
                Value::Null => {}
                v => config.automations.push(v.clone()),
            }
        } else if domain_key(key, "script") {
            match value {
                Value::Object(m) => config.scripts.extend(m.clone()),
                Value::Null => {}
                _ => return Err(format!("{key}: expected a dictionary of scripts")),
            }
        }
    }
    Ok(config)
}

/// `automation`, or `automation <label>` (HA lets one domain appear under several keys).
fn domain_key(key: &str, domain: &str) -> bool {
    key.strip_prefix(domain)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' ') && rest.len() > 1)
}

#[derive(Debug)]
pub struct YamlError {
    msg: String,
    /// The file itself is missing (HA re-raises FileNotFoundError, unlike other errors).
    not_found: bool,
}

fn err(msg: String) -> YamlError {
    YamlError {
        msg,
        not_found: false,
    }
}

/// `secrets.yaml` lookups: the including file's folder, then each parent up to the config dir.
struct Secrets {
    config_dir: PathBuf,
    cache: std::cell::RefCell<HashMap<PathBuf, Map<String, Value>>>,
}

impl Secrets {
    fn new(config_dir: &Path) -> Self {
        Self {
            config_dir: config_dir.into(),
            cache: Default::default(),
        }
    }

    fn get(&self, requester: &Path, name: &str) -> Result<Value, YamlError> {
        let mut dir = requester.parent();
        while let Some(d) = dir.filter(|d| d.starts_with(&self.config_dir)) {
            if let Some(v) = self.load_dir(d)?.get(name) {
                return Ok(v.clone());
            }
            dir = d.parent();
        }
        Err(err(format!("Secret {name} not defined")))
    }

    fn load_dir(&self, dir: &Path) -> Result<Map<String, Value>, YamlError> {
        let path = dir.join("secrets.yaml");
        if let Some(m) = self.cache.borrow().get(&path) {
            return Ok(m.clone());
        }
        let mut secrets = match load_yaml(&path, None) {
            Ok(Value::Object(m)) => m,
            Ok(_) => {
                return Err(err(format!(
                    "{}: Secrets is not a dictionary",
                    path.display()
                )));
            }
            Err(e) if e.not_found => Map::new(),
            Err(e) => return Err(e),
        };
        // HA reads `logger: debug` here to log secret lookups; it's not a secret.
        secrets.remove("logger");
        self.cache.borrow_mut().insert(path, secrets.clone());
        Ok(secrets)
    }
}

/// One YAML file without `!secret` (HA's `util.yaml.load_yaml` with no secrets), as the config
/// editors read `automations.yaml` and `scripts.yaml`; `None` if it doesn't exist.
pub fn load_file(path: &Path) -> Result<Option<Value>, String> {
    match load_yaml(path, None) {
        Ok(v) => Ok(Some(v)),
        Err(e) if e.not_found => Ok(None),
        Err(e) => Err(e.msg),
    }
}

/// One YAML file, as JSON. An empty file is null.
fn load_yaml(path: &Path, secrets: Option<&Secrets>) -> Result<Value, YamlError> {
    let text = std::fs::read_to_string(path).map_err(|e| YamlError {
        not_found: e.kind() == std::io::ErrorKind::NotFound,
        msg: format!("{}: {e}", path.display()),
    })?;
    Loader {
        path,
        secrets,
        anchors: HashMap::new(),
    }
    .document(&text)
    .map_err(|e| err(format!("{}: {}", path.display(), e.msg)))
}

struct Loader<'a> {
    path: &'a Path,
    secrets: Option<&'a Secrets>,
    anchors: HashMap<usize, Value>,
}

type Events<'i> = std::iter::Peekable<Parser<'i, saphyr_parser::StrInput<'i>>>;

impl Loader<'_> {
    fn document(&mut self, text: &str) -> Result<Value, YamlError> {
        let mut events: Events = Parser::new_from_str(text).peekable();
        let mut out = Value::Null;
        let mut seen_doc = false;
        while let Some(ev) = events.next() {
            let (ev, span) = ev.map_err(|e| err(e.to_string()))?;
            match ev {
                Event::StreamStart | Event::DocumentEnd | Event::Nothing => {}
                Event::StreamEnd => break,
                Event::DocumentStart(_) if seen_doc => {
                    return Err(err(format!(
                        "line {}: expected a single document in the stream",
                        span.start.line()
                    )));
                }
                Event::DocumentStart(_) => {
                    seen_doc = true;
                    // An empty document has no node before DocumentEnd.
                    if !matches!(events.peek(), Some(Ok((Event::DocumentEnd, _)))) {
                        out = self.node(&mut events)?;
                    }
                }
                _ => out = self.value(ev, span.start.line(), &mut events)?,
            }
        }
        Ok(out)
    }

    fn node(&mut self, events: &mut Events) -> Result<Value, YamlError> {
        let (ev, span) = events
            .next()
            .ok_or_else(|| err("unexpected end of file".into()))?
            .map_err(|e| err(e.to_string()))?;
        self.value(ev, span.start.line(), events)
    }

    fn value(&mut self, ev: Event, line: usize, events: &mut Events) -> Result<Value, YamlError> {
        let at = |msg: String| err(format!("line {line}: {msg}"));
        let (v, anchor) = match ev {
            Event::Alias(id) => {
                return self
                    .anchors
                    .get(&id)
                    .cloned()
                    .ok_or_else(|| at("found undefined alias".into()));
            }
            Event::Scalar(text, style, anchor, tag) => (
                self.scalar(&text, style, tag.as_deref())
                    .map_err(|e| at(e.msg))?,
                anchor,
            ),
            Event::SequenceStart(anchor, tag) => {
                let mut items = vec![];
                while !matches!(events.peek(), Some(Ok((Event::SequenceEnd, _)))) {
                    items.push(self.node(events)?);
                }
                events.next();
                (
                    self.tagged_collection(Value::Array(items), tag.as_deref())
                        .map_err(|e| at(e.msg))?,
                    anchor,
                )
            }
            Event::MappingStart(anchor, tag) => {
                let mut map = Map::new();
                // `<<` merges: keys of this mapping win, then the merged ones in list order.
                let mut merged: Vec<Map<String, Value>> = vec![];
                while !matches!(events.peek(), Some(Ok((Event::MappingEnd, _)))) {
                    let key = self.node(events)?;
                    let val = self.node(events)?;
                    if key == "<<" {
                        match val {
                            Value::Object(m) => merged.push(m),
                            Value::Array(a) => {
                                for m in a {
                                    let Value::Object(m) = m else {
                                        return Err(at("expected a mapping for merging".into()));
                                    };
                                    merged.push(m);
                                }
                            }
                            _ => {
                                return Err(at(
                                    "expected a mapping or list of mappings for merging".into(),
                                ));
                            }
                        }
                        continue;
                    }
                    let key = match key {
                        Value::String(s) => s,
                        // shortcut: non-string keys (`1:`, `on:`) become Python's str() of them;
                        // JSON has only string keys, and HA configs rarely use others.
                        Value::Bool(b) => if b { "True" } else { "False" }.into(),
                        Value::Null => "None".into(),
                        Value::Number(n) => n.to_string(),
                        _ => return Err(at("unhashable key".into())),
                    };
                    if map.contains_key(&key) {
                        // HA warns and keeps the last value.
                        eprintln!(
                            "config: {} contains duplicate key {key:?}",
                            self.path.display()
                        );
                        map.shift_remove(&key);
                    }
                    map.insert(key, val);
                }
                events.next();
                for m in merged {
                    for (k, v) in m {
                        map.entry(k).or_insert(v);
                    }
                }
                (
                    self.tagged_collection(Value::Object(map), tag.as_deref())
                        .map_err(|e| at(e.msg))?,
                    anchor,
                )
            }
            _ => return Err(at("unexpected YAML event".into())),
        };
        if anchor != 0 {
            self.anchors.insert(anchor, v.clone());
        }
        Ok(v)
    }

    fn tagged_collection(&self, v: Value, tag: Option<&Tag>) -> Result<Value, YamlError> {
        match tag {
            None => Ok(v),
            Some(t)
                if t.handle == "tag:yaml.org,2002:"
                    && matches!(t.suffix.as_str(), "seq" | "map") =>
            {
                Ok(v)
            }
            Some(t) => Err(err(format!(
                "could not determine a constructor for the tag '{}{}'",
                t.handle, t.suffix
            ))),
        }
    }

    fn scalar(
        &mut self,
        text: &str,
        style: ScalarStyle,
        tag: Option<&Tag>,
    ) -> Result<Value, YamlError> {
        let Some(tag) = tag else {
            return Ok(if style == ScalarStyle::Plain {
                resolve_plain(text)
            } else {
                Value::String(text.into())
            });
        };
        if tag.handle == "tag:yaml.org,2002:" {
            return match tag.suffix.as_str() {
                "str" => Ok(text.into()),
                "null" | "bool" | "int" | "float" | "timestamp" => Ok(resolve_plain(text)),
                s => Err(err(format!(
                    "could not determine a constructor for the tag '!!{s}'"
                ))),
            };
        }
        if tag.handle != "!" {
            return Err(err(format!(
                "could not determine a constructor for the tag '{}{}'",
                tag.handle, tag.suffix
            )));
        }
        let name = tag.suffix.as_str();
        if text.is_empty() && name != "input" {
            return Err(err(format!("!{name} needs an argument.")));
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        match name {
            "include" => {
                let file = dir.join(text);
                match load_yaml(&file, self.secrets) {
                    Ok(Value::Null) => Ok(Value::Object(Map::new())),
                    Ok(v) => Ok(v),
                    Err(e) if e.not_found => {
                        Err(err(format!("Unable to read file {}", file.display())))
                    }
                    Err(e) => Err(e),
                }
            }
            "include_dir_list"
            | "include_dir_merge_list"
            | "include_dir_named"
            | "include_dir_merge_named" => self.include_dir(&dir.join(text), name),
            "secret" => match self.secrets {
                Some(s) => s.get(self.path, text),
                None => Err(err("Secrets not supported in this YAML file".into())),
            },
            "env_var" => {
                let mut args = text.split_whitespace();
                let var = args.next().unwrap_or_default();
                let default: Vec<&str> = args.collect();
                match std::env::var(var) {
                    Ok(v) => Ok(v.into()),
                    Err(_) if !default.is_empty() => Ok(default.join(" ").into()),
                    Err(_) => Err(err(format!("Environment variable {var} not defined"))),
                }
            }
            // Blueprint inputs, substituted when a blueprint is used (not loaded yet).
            "input" => Ok(json!({"!input": text})),
            _ => Err(err(format!(
                "could not determine a constructor for the tag '!{name}'"
            ))),
        }
    }

    fn include_dir(&self, dir: &Path, kind: &str) -> Result<Value, YamlError> {
        let mut files = vec![];
        find_yaml_files(dir, &mut files);
        let mut list = vec![];
        let mut map = Map::new();
        for f in files {
            let v = load_yaml(&f, self.secrets)?;
            match (kind, v) {
                ("include_dir_list", Value::Null) => {}
                ("include_dir_list", v) => list.push(v),
                ("include_dir_merge_list", Value::Array(a)) => list.extend(a),
                ("include_dir_named", v) => {
                    let stem = f
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or_default()
                        .to_string();
                    // An empty file is an empty dict here.
                    map.insert(
                        stem,
                        if v.is_null() {
                            Value::Object(Map::new())
                        } else {
                            v
                        },
                    );
                }
                ("include_dir_merge_named", Value::Object(m)) => map.extend(m),
                _ => {}
            }
        }
        Ok(if kind.ends_with("list") {
            list.into()
        } else {
            map.into()
        })
    }
}

/// HA's `_find_files(dir, "*.yaml")`: recursive, skipping dotfiles and dot-dirs, files sorted
/// within each folder; `secrets.yaml` is never included.
fn find_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    let mut entries: Vec<_> = rd.flatten().map(|e| e.path()).collect();
    entries.sort();
    let visible = |p: &Path| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| !n.starts_with('.'))
    };
    let (dirs, files): (Vec<_>, Vec<_>) = entries
        .into_iter()
        .filter(|p| visible(p))
        .partition(|p| p.is_dir());
    out.extend(files.into_iter().filter(|f| {
        let name = f.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        name.ends_with(".yaml") && name != "secrets.yaml"
    }));
    for d in dirs {
        find_yaml_files(&d, out);
    }
}

/// Whether PyYAML reads `s`, written plain, back as a string: what decides whether the dumper
/// must quote it. Its resolver also knows `=` and `<<`, and reads any run of digits as an int,
/// however long.
pub(crate) fn plain_is_str(s: &str) -> bool {
    let (_, digits) = split_sign(s);
    let decimal = digits.starts_with(|c: char| ('1'..='9').contains(&c))
        && digits.chars().all(|c| c.is_ascii_digit() || c == '_');
    let special = matches!(resolve_plain(s), Value::Null | Value::Bool(_))
        || matches!(
            s,
            "=" | "<<"
                | ".inf"
                | ".Inf"
                | ".INF"
                | "+.inf"
                | "+.Inf"
                | "+.INF"
                | "-.inf"
                | "-.Inf"
                | "-.INF"
                | ".nan"
                | ".NaN"
                | ".NAN"
        );
    !(special
        || decimal
        || yaml11_int(s).is_some()
        || yaml11_float(s).is_some()
        || yaml11_timestamp(s).is_some())
}

/// PyYAML's implicit resolvers (YAML 1.1) for an untagged plain scalar.
fn resolve_plain(s: &str) -> Value {
    match s {
        "" | "~" | "null" | "Null" | "NULL" => return Value::Null,
        "yes" | "Yes" | "YES" | "true" | "True" | "TRUE" | "on" | "On" | "ON" => {
            return true.into();
        }
        "no" | "No" | "NO" | "false" | "False" | "FALSE" | "off" | "Off" | "OFF" => {
            return false.into();
        }
        // shortcut: infinity and NaN become strings; JSON has no such numbers, and configs
        // almost never use them.
        ".inf" | ".Inf" | ".INF" | "+.inf" | "+.Inf" | "+.INF" => {
            return f64::INFINITY.to_string().into();
        }
        "-.inf" | "-.Inf" | "-.INF" => return f64::NEG_INFINITY.to_string().into(),
        ".nan" | ".NaN" | ".NAN" => return "nan".into(),
        _ => {}
    }
    if let Some(i) = yaml11_int(s) {
        return i.into();
    }
    if let Some(f) = yaml11_float(s) {
        return f.into();
    }
    if let Some(t) = yaml11_timestamp(s) {
        return t.into();
    }
    s.into()
}

fn split_sign(s: &str) -> (i64, &str) {
    match s.as_bytes().first() {
        Some(b'-') => (-1, &s[1..]),
        Some(b'+') => (1, &s[1..]),
        _ => (1, s),
    }
}

/// `[-+]?(0b[01_]+ | 0[0-7_]+ | 0|[1-9][0-9_]* | 0x[0-9a-fA-F_]+ | [1-9][0-9_]*(:[0-5]?[0-9])+)`
fn yaml11_int(s: &str) -> Option<i64> {
    let (sign, body) = split_sign(s);
    let digits =
        |b: &str, ok: fn(char) -> bool| !b.is_empty() && b.chars().all(|c| ok(c) || c == '_');
    let clean = |b: &str| b.replace('_', "");
    let v = if let Some(b) = body.strip_prefix("0b") {
        digits(b, |c| c == '0' || c == '1').then(|| i64::from_str_radix(&clean(b), 2).ok())??
    } else if let Some(h) = body.strip_prefix("0x") {
        digits(h, |c| c.is_ascii_hexdigit()).then(|| i64::from_str_radix(&clean(h), 16).ok())??
    } else if body == "0" {
        0
    } else if let Some(o) = body.strip_prefix('0') {
        digits(o, |c| ('0'..='7').contains(&c))
            .then(|| i64::from_str_radix(&clean(o), 8).ok())??
    } else if body.contains(':') {
        let mut parts = body.split(':');
        let first = parts.next()?;
        if !first.starts_with(|c: char| ('1'..='9').contains(&c))
            || !digits(first, |c| c.is_ascii_digit())
        {
            return None;
        }
        let mut v: i64 = clean(first).parse().ok()?;
        for p in parts {
            if p.is_empty()
                || p.len() > 2
                || !p.chars().all(|c| c.is_ascii_digit())
                || p.parse::<u8>().ok()? > 59
            {
                return None;
            }
            v = v * 60 + p.parse::<i64>().ok()?;
        }
        v
    } else if body.starts_with(|c: char| ('1'..='9').contains(&c))
        && digits(body, |c| c.is_ascii_digit())
    {
        clean(body).parse().ok()?
    } else {
        return None;
    };
    Some(sign * v)
}

/// `[-+]?([0-9][0-9_]*)?\.[0-9_]*([eE][-+][0-9]+)?` and base 60 (`1:30.5`). Note the exponent
/// needs a sign and a dot: `1e5` is a string in YAML 1.1.
fn yaml11_float(s: &str) -> Option<f64> {
    let (sign, body) = split_sign(s);
    if body.contains(':') {
        let (whole, frac) = body.rsplit_once('.')?;
        if !frac.chars().all(|c| c.is_ascii_digit() || c == '_') {
            return None;
        }
        let mut parts = whole.split(':');
        let first = parts.next()?;
        if first.is_empty() || !first.chars().all(|c| c.is_ascii_digit() || c == '_') {
            return None;
        }
        let mut v: f64 = first.replace('_', "").parse().ok()?;
        for p in parts {
            if p.is_empty()
                || p.len() > 2
                || !p.chars().all(|c| c.is_ascii_digit())
                || p.parse::<u8>().ok()? > 59
            {
                return None;
            }
            v = v * 60.0 + p.parse::<f64>().ok()?;
        }
        let f: f64 = format!("0.{}", frac.replace('_', ""))
            .parse()
            .unwrap_or(0.0);
        return Some(sign as f64 * (v + f));
    }
    let (mantissa, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], Some(&body[i + 1..])),
        None => (body, None),
    };
    let (int, frac) = mantissa.split_once('.')?;
    let int_ok = int.is_empty()
        || (int.starts_with(|c: char| c.is_ascii_digit())
            && int.chars().all(|c| c.is_ascii_digit() || c == '_'));
    let frac_ok = frac.chars().all(|c| c.is_ascii_digit() || c == '_');
    // `.5` needs a digit after the dot; `1.` is fine.
    if !int_ok
        || !frac_ok
        || (int.is_empty() && frac.is_empty())
        || (int.is_empty() && !frac.starts_with(|c: char| c.is_ascii_digit()))
    {
        return None;
    }
    if let Some(e) = exp {
        let digits = e.strip_prefix(['-', '+'])?;
        if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
            return None;
        }
    }
    let f: f64 = body.replace('_', "").parse().ok()?;
    Some(sign as f64 * f)
}

/// `YYYY-MM-DD` or a date and time: PyYAML makes these `date`/`datetime` objects; here they
/// are Python's `str()` of them, which is what HA's string validators then see.
// shortcut: as strings, not date objects; HA's own configs quote dates where they matter.
fn yaml11_timestamp(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let ymd = |t: &str| -> Option<chrono::NaiveDate> {
        let mut p = t.split('-');
        let (y, m, d) = (p.next()?, p.next()?, p.next()?);
        if p.next().is_some()
            || y.len() != 4
            || !(1..=2).contains(&m.len())
            || !(1..=2).contains(&d.len())
        {
            return None;
        }
        chrono::NaiveDate::from_ymd_opt(y.parse().ok()?, m.parse().ok()?, d.parse().ok()?)
    };
    if b.len() == 10 && b[4] == b'-' && b[7] == b'-' {
        return ymd(s).map(|d| d.to_string());
    }
    let sep = s.find(['T', 't', ' ', '\t'])?;
    let date = ymd(&s[..sep])?;
    let rest = s[sep..].trim_start_matches(['T', 't', ' ', '\t']);
    let (time, zone) = match rest.find(['Z', '+', '-', ' ']) {
        Some(i) => (&rest[..i], rest[i..].trim()),
        None => (rest, ""),
    };
    let t = chrono::NaiveTime::parse_from_str(time, "%H:%M:%S%.f").ok()?;
    let dt = date.and_time(t);
    Some(match zone {
        "" => dt.to_string(),
        "Z" => format!("{dt}+00:00"),
        z => {
            let z = if z.len() <= 3 {
                format!("{z}:00")
            } else {
                z.to_string()
            };
            let z = if z.len() == 5 {
                format!("{}0{}", &z[..1], &z[1..])
            } else {
                z
            };
            format!("{dt}{z}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(files: &[(&str, &str)]) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("lean-home-config-{}", crate::auth::random_hex(8)));
        for (name, text) in files {
            let p = d.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, text).unwrap();
        }
        d
    }

    fn parse(text: &str) -> Value {
        let d = dir(&[("x.yaml", text)]);
        let v = load_yaml(&d.join("x.yaml"), Some(&Secrets::new(&d))).unwrap();
        std::fs::remove_dir_all(d).ok();
        v
    }

    #[test]
    fn yaml_1_1_scalars_like_pyyaml() {
        // Expected values are real PyYAML's, through HA's own loader (HA 2026.11.2 image).
        let v = parse(
            "a: on\nb: 'on'\nc: yes\nd: Off\ne: ~\nf:\ng: 0x1F\nh: 0777\ni: 1_000\nj: 1:30\nk: 1.5\nl: 1e5\nm: 1.0e+3\nn: .5\no: 03:00:00\no2: 3:00:00\np: 2026-11-24\nq: \"12\"\nr: 012\ns: !!str 12\nt: -7\nu: 2026-11-24 10:00:00Z\nv: 1.5:30.2\nw: 08\nx: 0b101\ny: 1:61\n",
        );
        assert_eq!(
            v,
            json!({"a": true, "b": "on", "c": true, "d": false, "e": null, "f": null, "g": 31, "h": 511,
                "i": 1000, "j": 90, "k": 1.5, "l": "1e5", "m": 1000.0, "n": 0.5,
                // base 60 only without a leading zero: an unquoted 3:00:00 is 3*3600
                "o": "03:00:00", "o2": 10800, "p": "2026-11-24", "q": "12", "r": 10, "s": "12", "t": -7,
                "u": "2026-11-24 10:00:00+00:00", "v": "1.5:30.2", "w": "08", "x": 5, "y": "1:61"})
        );
    }

    #[test]
    fn anchors_merge_keys_and_duplicates() {
        let v = parse("base: &b {x: 1, y: 2}\nc:\n  <<: *b\n  y: 3\nl: [*b]\nd: 1\nd: 2\n");
        assert_eq!(v["c"], json!({"y": 3, "x": 1}));
        assert_eq!(v["l"], json!([{"x": 1, "y": 2}]));
        assert_eq!(v["d"], 2);
    }

    #[test]
    fn includes_and_secrets() {
        let d = dir(&[
            (
                "configuration.yaml",
                "homeassistant:\n  time_zone: Europe/Amsterdam\n  name: !secret home_name\nautomation: !include automations.yaml\nautomation extra: !include_dir_merge_list more\nscript: !include_dir_merge_named scripts\nsensor: !include_dir_list sensors\nempty: !include empty.yaml\nnamed: !include_dir_named sensors\nenv: !env_var LEAN_HOME_TEST_UNSET fallback value\n",
            ),
            ("secrets.yaml", "home_name: Cottage\nlogger: debug\n"),
            ("automations.yaml", "- id: '1'\n  alias: First\n"),
            ("more/b.yaml", "- id: '3'\n"),
            ("more/a.yaml", "- id: '2'\n"),
            ("more/.hidden.yaml", "- id: 'nope'\n"),
            ("scripts/s.yaml", "blink:\n  sequence: []\n"),
            (
                "scripts/sub/t.yaml",
                "wave:\n  sequence: []\n  secret: !secret nested\n",
            ),
            ("scripts/sub/secrets.yaml", "nested: from-sub\n"),
            ("sensors/one.yaml", "platform: x\n"),
            ("sensors/two.yml", "platform: ignored\n"),
            ("empty.yaml", ""),
        ]);
        let c = load(&d).unwrap();
        assert_eq!(c.core.location_name, "Cottage");
        assert_eq!(c.core.time_zone, "Europe/Amsterdam");
        assert_eq!(c.core.config_source, "yaml");
        let ids: Vec<_> = c
            .automations
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        // file order, then the dir in sorted order, dotfiles skipped
        assert_eq!(ids, ["1", "2", "3"]);
        assert_eq!(c.scripts.keys().collect::<Vec<_>>(), ["blink", "wave"]);
        assert_eq!(c.scripts["wave"]["secret"], "from-sub");
        let root = load_yaml(&d.join("configuration.yaml"), Some(&Secrets::new(&d))).unwrap();
        assert_eq!(root["sensor"], json!([{"platform": "x"}]));
        assert_eq!(root["empty"], json!({}));
        assert_eq!(root["named"], json!({"one": {"platform": "x"}}));
        assert_eq!(root["env"], "fallback value");

        std::fs::write(d.join("configuration.yaml"), "x: !secret missing\n").unwrap();
        assert!(load(&d).unwrap_err().contains("Secret missing not defined"));
        std::fs::write(d.join("configuration.yaml"), "x: !include nope.yaml\n").unwrap();
        assert!(load(&d).unwrap_err().contains("Unable to read file"));
        std::fs::write(d.join("configuration.yaml"), "x: [unclosed\n").unwrap();
        assert!(load(&d).is_err());
        std::fs::remove_dir_all(d).ok();
    }

    #[test]
    fn core_config_precedence() {
        let none = dir(&[]);
        let c = load(&none).unwrap();
        assert_eq!(c.core, Core::default());
        assert_eq!(c.core.config_source, "default");

        let storage = r#"{"version":1,"minor_version":4,"key":"core.config","data":{"latitude":52.37,"longitude":4.89,"elevation":2,"unit_system_v2":"us_customary","location_name":"Flat","time_zone":"America/New_York","external_url":null,"internal_url":"http://hub.local:8123","currency":"USD","country":"US","language":"en","radius":50}}"#;
        let d = dir(&[
            (".storage/core.config", storage),
            ("configuration.yaml", "default_config:\n"),
        ]);
        let c = load(&d).unwrap().core;
        assert_eq!(
            (
                c.config_source,
                c.time_zone.as_str(),
                c.unit_system.as_str()
            ),
            ("storage", "America/New_York", "us_customary")
        );
        assert_eq!(
            (c.latitude.clone(), c.radius.clone(), c.internal_url.clone()),
            (json!(52.37), json!(50), json!("http://hub.local:8123"))
        );
        assert_eq!(c.units_json()["temperature"], "°F");

        std::fs::write(
            d.join("configuration.yaml"),
            "homeassistant:\n  time_zone: Europe/Berlin\n",
        )
        .unwrap();
        let c = load(&d).unwrap().core;
        assert_eq!(
            (c.config_source, c.time_zone.as_str(), c.location_name),
            ("yaml", "Europe/Berlin", json!("Flat"))
        );
        std::fs::write(
            d.join("configuration.yaml"),
            "homeassistant:\n  time_zone: Mars/Base\n",
        )
        .unwrap();
        assert!(load(&d).unwrap_err().contains("invalid time zone"));
        std::fs::remove_dir_all(d).ok();
        std::fs::remove_dir_all(none).ok();
    }

    /// The recorded scenario's config, against what HA's own loader made of it.
    #[test]
    fn scenario_config_matches_ha() {
        let compat = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../compat");
        let dir = compat.join("automations");
        let ours = load_yaml(&dir.join("configuration.yaml"), Some(&Secrets::new(&dir))).unwrap();
        let ha: Value = serde_json::from_str(
            &std::fs::read_to_string(compat.join("fixtures/automations/configuration.json"))
                .unwrap(),
        )
        .unwrap();
        assert_eq!(ours, ha);
        let c = load(&dir).unwrap();
        assert_eq!((c.automations.len(), c.scripts.len()), (3, 1));
    }

    #[test]
    fn zone_matches_python_zoneinfo() {
        let z = Zone::load("Europe/Amsterdam").unwrap();
        let ts = |y, mo, d, h, mi| {
            let n = chrono::NaiveDate::from_ymd_opt(y, mo, d)
                .unwrap()
                .and_hms_opt(h, mi, 0)
                .unwrap();
            z.local_to_utc(n).timestamp()
        };
        // Expected: Python's datetime(..., tzinfo=ZoneInfo("Europe/Amsterdam")).timestamp()
        assert_eq!(ts(2026, 1, 15, 8, 0), 1768460400);
        assert_eq!(ts(2026, 7, 1, 12, 0), 1782900000);
        assert_eq!(ts(2026, 3, 29, 2, 30), 1774747800); // in the spring-forward gap
        assert_eq!(ts(2026, 10, 25, 2, 30), 1792888200); // in the autumn overlap: first one
        let summer = chrono::DateTime::from_timestamp(1782900000, 0).unwrap();
        assert_eq!(z.offset(summer).local_minus_utc(), 7200);
        assert_eq!(Zone::utc().offset(summer).local_minus_utc(), 0);
        assert!(Zone::load("../../etc/passwd").is_err());
        assert!(Zone::load("Mars/Base").is_err());
    }

    #[test]
    fn domain_keys() {
        assert!(domain_key("automation", "automation"));
        assert!(domain_key("automation manual", "automation"));
        assert!(!domain_key("automations", "automation"));
        assert!(!domain_key("automation ", "automation"));
    }
}
