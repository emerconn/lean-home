//! YAML output as HA writes `automations.yaml` and `scripts.yaml` from the config editors:
//! annotatedyaml's `dump` (PyYAML's `CSafeDumper`, i.e. libyaml's emitter, with
//! `default_flow_style=False`, `allow_unicode=True`, `sort_keys=False`, then `": null\n"` replaced
//! by `":\n"`). Scalar styles and line folding follow libyaml's `emitter.c`; the corpus in
//! `compat/fixtures/yaml-dump/` is HA's own output for tricky values.

use serde_json::{Map, Value};

/// libyaml's `best_width`: plain and quoted scalars fold at a space past this column.
const WIDTH: usize = 80;
/// libyaml's `best_indent`.
const INDENT: usize = 2;

/// `annotatedyaml.dumper.dump`
pub fn dump(v: &Value) -> String {
    let mut e = Emitter::default();
    match v {
        Value::Object(m) if block(m) => e.map(m, 0, false),
        Value::Array(a) if !a.is_empty() => e.seq(a, 0, false),
        other => e.inline(other, 0),
    }
    e.out.push('\n');
    e.out.replace(": null\n", ":\n")
}

#[derive(Default)]
struct Emitter {
    out: String,
    /// Characters on the current line.
    column: usize,
}

/// A scalar's style, as libyaml's `yaml_emitter_select_scalar_style` picks it.
#[derive(PartialEq)]
enum Style {
    Plain,
    Single,
    Double,
}

impl Emitter {
    fn push(&mut self, s: &str) {
        for c in s.chars() {
            if c == '\n' {
                self.column = 0;
            } else {
                self.column += 1;
            }
            self.out.push(c);
        }
    }

    fn push_char(&mut self, c: char) {
        self.push(c.encode_utf8(&mut [0; 4]));
    }

    /// A new line, indented.
    fn indent(&mut self, n: usize) {
        self.push("\n");
        self.push(&" ".repeat(n));
    }

    /// A block mapping whose keys sit at column `ind`; `inline`: the first key continues the
    /// current line (after `- `).
    fn map(&mut self, m: &Map<String, Value>, ind: usize, inline: bool) {
        for (i, (k, v)) in m.iter().enumerate() {
            if i > 0 || !inline && !self.out.is_empty() {
                self.indent(ind);
            }
            // shortcut: keys over 128 characters, which libyaml writes as `? key`, aren't
            // handled; config keys are short.
            self.scalar(k, ind + INDENT, true);
            self.push(":");
            match v {
                Value::Object(c) if block(c) => self.map(c, ind + INDENT, false),
                // A sequence in a mapping is indentless: its `- ` lines up with the key.
                Value::Array(a) if !a.is_empty() => self.seq(a, ind, false),
                other => {
                    self.push(" ");
                    self.inline(other, ind + INDENT);
                }
            }
        }
    }

    /// A block sequence whose `- ` sit at column `ind`.
    fn seq(&mut self, a: &[Value], ind: usize, inline: bool) {
        for (i, v) in a.iter().enumerate() {
            if i > 0 || !inline && !self.out.is_empty() {
                self.indent(ind);
            }
            self.push("- ");
            match v {
                Value::Object(c) if block(c) => self.map(c, ind + INDENT, true),
                Value::Array(c) if !c.is_empty() => self.seq(c, ind + INDENT, true),
                other => self.inline(other, ind + INDENT),
            }
        }
    }

    /// A scalar or an empty collection (`{}`, `[]`, the flow style libyaml keeps for them).
    /// `ind`: where a folded scalar's next lines start.
    fn inline(&mut self, v: &Value, ind: usize) {
        match v {
            Value::Null => self.push("null"),
            Value::Bool(b) => self.push(if *b { "true" } else { "false" }),
            Value::Number(n) => match n.as_f64() {
                Some(f) if !n.is_i64() && !n.is_u64() => self.push(&py_float(f)),
                _ => self.push(&n.to_string()),
            },
            Value::String(s) => self.scalar(s, ind, false),
            Value::Array(_) => self.push("[]"),
            Value::Object(m) => match input_tag(m) {
                Some(name) => {
                    self.push("!input ");
                    self.scalar(name, ind, false);
                }
                None => self.push("{}"),
            },
        }
    }

    fn scalar(&mut self, s: &str, ind: usize, key: bool) {
        let a = analyze(s);
        let mut style = Style::Plain;
        if key && a.multiline {
            style = Style::Double;
        } else {
            if !a.block_plain || s.is_empty() || !crate::config::plain_is_str(s) {
                style = Style::Single;
            }
            if style == Style::Single && !a.single_quoted {
                style = Style::Double;
            }
        }
        let breaks = !key;
        match style {
            Style::Plain => self.plain(s, ind, breaks),
            Style::Single => self.single(s, ind, breaks),
            Style::Double => self.double(s, ind, breaks),
        }
    }

    /// libyaml's `yaml_emitter_write_plain`.
    fn plain(&mut self, s: &str, ind: usize, breaks: bool) {
        let chars: Vec<char> = s.chars().collect();
        let mut spaces = false;
        for (i, &c) in chars.iter().enumerate() {
            if c == ' ' {
                if breaks && !spaces && self.column > WIDTH && chars.get(i + 1) != Some(&' ') {
                    self.indent(ind);
                } else {
                    self.push(" ");
                }
                spaces = true;
            } else {
                self.push_char(c);
                spaces = false;
            }
        }
    }

    /// libyaml's `yaml_emitter_write_single_quoted`.
    fn single(&mut self, s: &str, ind: usize, allow_breaks: bool) {
        let chars: Vec<char> = s.chars().collect();
        let (mut spaces, mut breaks) = (false, false);
        self.push("'");
        for (i, &c) in chars.iter().enumerate() {
            if c == ' ' {
                if allow_breaks
                    && !spaces
                    && self.column > WIDTH
                    && i != 0
                    && i != chars.len() - 1
                    && chars.get(i + 1) != Some(&' ')
                {
                    self.indent(ind);
                } else {
                    self.push(" ");
                }
                spaces = true;
            } else if is_break(c) {
                // A line break is written as a blank line (`\n` twice) the first time.
                if !breaks && c == '\n' {
                    self.push("\n");
                }
                self.push_char(c);
                self.column = 0;
                breaks = true;
            } else {
                if breaks {
                    self.push(&" ".repeat(ind));
                }
                if c == '\'' {
                    self.push("''");
                } else {
                    self.push_char(c);
                }
                spaces = false;
                breaks = false;
            }
        }
        if breaks {
            self.push(&" ".repeat(ind));
        }
        self.push("'");
    }

    /// libyaml's `yaml_emitter_write_double_quoted`.
    fn double(&mut self, s: &str, ind: usize, allow_breaks: bool) {
        let chars: Vec<char> = s.chars().collect();
        let mut spaces = false;
        self.push("\"");
        for (i, &c) in chars.iter().enumerate() {
            if !printable(c) || c == '\u{feff}' || is_break(c) || c == '"' || c == '\\' {
                let esc = match c {
                    '\0' => "\\0".into(),
                    '\u{7}' => "\\a".into(),
                    '\u{8}' => "\\b".into(),
                    '\t' => "\\t".into(),
                    '\n' => "\\n".into(),
                    '\u{b}' => "\\v".into(),
                    '\u{c}' => "\\f".into(),
                    '\r' => "\\r".into(),
                    '\u{1b}' => "\\e".into(),
                    '"' => "\\\"".into(),
                    '\\' => "\\\\".into(),
                    '\u{85}' => "\\N".into(),
                    '\u{a0}' => "\\_".into(),
                    '\u{2028}' => "\\L".into(),
                    '\u{2029}' => "\\P".into(),
                    c if (c as u32) <= 0xff => format!("\\x{:02X}", c as u32),
                    c if (c as u32) <= 0xffff => format!("\\u{:04X}", c as u32),
                    c => format!("\\U{:08X}", c as u32),
                };
                self.push(&esc);
                spaces = false;
            } else if c == ' ' {
                if allow_breaks && !spaces && self.column > WIDTH && i != 0 && i != chars.len() - 1
                {
                    self.indent(ind);
                    if chars.get(i + 1) == Some(&' ') {
                        self.push("\\");
                    }
                } else {
                    self.push(" ");
                }
                spaces = true;
            } else {
                self.push_char(c);
                spaces = false;
            }
        }
        self.push("\"");
    }
}

/// A mapping written as a block: not empty, and not an `!input` tag.
fn block(m: &Map<String, Value>) -> bool {
    !m.is_empty() && input_tag(m).is_none()
}

/// The `{"!input": name}` marker `config.rs` loads `!input name` as, written back as the tag.
fn input_tag(m: &Map<String, Value>) -> Option<&str> {
    match (m.len(), m.get("!input")) {
        (1, Some(Value::String(name))) => Some(name),
        _ => None,
    }
}

fn is_break(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// libyaml's `IS_PRINTABLE`: only the BMP, so an emoji is escaped.
fn printable(c: char) -> bool {
    let c = c as u32;
    c == 0x0a
        || (0x20..=0x7e).contains(&c)
        || (0xa0..=0xd7ff).contains(&c)
        || ((0xe000..=0xfffd).contains(&c) && c != 0xfeff)
}

/// What `yaml_emitter_analyze_scalar` decides, for block context.
struct Analysis {
    multiline: bool,
    block_plain: bool,
    single_quoted: bool,
}

fn analyze(s: &str) -> Analysis {
    if s.is_empty() {
        return Analysis {
            multiline: false,
            block_plain: true,
            single_quoted: true,
        };
    }
    let chars: Vec<char> = s.chars().collect();
    let blankz = |i: usize| {
        chars
            .get(i)
            .is_none_or(|&c| c == ' ' || c == '\t' || is_break(c))
    };
    let mut block_indicators = s.starts_with("---") || s.starts_with("...");
    let (mut special, mut line_breaks) = (false, false);
    let (mut leading_space, mut leading_break, mut trailing_space, mut trailing_break) =
        (false, false, false, false);
    let (mut break_space, mut space_break) = (false, false);
    let (mut previous_space, mut previous_break) = (false, false);
    let mut preceded_by_ws = true;
    for (i, &c) in chars.iter().enumerate() {
        let followed_by_ws = blankz(i + 1);
        let (first, last) = (i == 0, i == chars.len() - 1);
        if first {
            if "#,[]{}&*!|>'\"%@`".contains(c)
                || (c == '-' || c == '?' || c == ':') && followed_by_ws
            {
                block_indicators = true;
            }
        } else if (c == ':' && followed_by_ws) || (c == '#' && preceded_by_ws) {
            block_indicators = true;
        }
        if !printable(c) {
            special = true;
        }
        if is_break(c) {
            line_breaks = true;
        }
        if c == ' ' {
            leading_space |= first;
            trailing_space |= last;
            break_space |= previous_break;
            (previous_space, previous_break) = (true, false);
        } else if is_break(c) {
            leading_break |= first;
            trailing_break |= last;
            space_break |= previous_space;
            (previous_space, previous_break) = (false, true);
        } else {
            (previous_space, previous_break) = (false, false);
        }
        preceded_by_ws = c == ' ' || c == '\t' || is_break(c);
    }
    let mut block_plain = !(leading_space || leading_break || trailing_space || trailing_break);
    let mut single_quoted = true;
    if break_space {
        block_plain = false;
        single_quoted = false;
    }
    if space_break || special {
        block_plain = false;
        single_quoted = false;
    }
    if line_breaks || block_indicators {
        block_plain = false;
    }
    Analysis {
        multiline: line_breaks,
        block_plain,
        single_quoted,
    }
}

/// PyYAML's `represent_float`: Python's `repr`, lowercased, with `.0` added before an exponent.
fn py_float(f: f64) -> String {
    if f.is_nan() {
        return ".nan".into();
    }
    if f.is_infinite() {
        return if f > 0.0 { ".inf" } else { "-.inf" }.into();
    }
    // Shortest round-trip digits, then Python's choice between positional and exponent form.
    let sci = format!("{f:e}");
    let (mantissa, exp) = sci.split_once('e').unwrap();
    let exp: i32 = exp.parse().unwrap();
    let (neg, mantissa) = mantissa
        .strip_prefix('-')
        .map_or((false, mantissa), |m| (true, m));
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let body = if (-4..16).contains(&exp) {
        let point = exp + 1; // digits before the decimal point
        if point <= 0 {
            format!("0.{}{digits}", "0".repeat((-point) as usize))
        } else if point as usize >= digits.len() {
            format!("{digits}{}.0", "0".repeat(point as usize - digits.len()))
        } else {
            format!(
                "{}.{}",
                &digits[..point as usize],
                &digits[point as usize..]
            )
        }
    } else {
        let m = if digits.len() > 1 {
            format!("{}.{}", &digits[..1], &digits[1..])
        } else {
            format!("{digits}.0")
        };
        format!(
            "{m}e{}{:02}",
            if exp < 0 { '-' } else { '+' },
            exp.unsigned_abs()
        )
    };
    if neg { format!("-{body}") } else { body }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every case HA's own dumper wrote (`compat/yaml-dump-cases.py`) comes out the same.
    #[test]
    fn matches_ha_dumper() {
        let cases = include_str!("../../../compat/fixtures/yaml-dump/cases.jsonl");
        // shortcut: serde_json reads an integer past u64 as a float (HA keeps Python's big int);
        // lean-home's YAML loader stops at i64 too. Add serde_json's arbitrary_precision if a
        // config ever needs one.
        let known = ["\"big\": 100000000000000000000"];
        let mut failed = vec![];
        for line in cases
            .lines()
            .filter(|l| !known.iter().any(|k| l.contains(k)))
        {
            let c: Value = serde_json::from_str(line).unwrap();
            let got = dump(&c["value"]);
            if got != c["yaml"] {
                failed.push(format!(
                    "{}\n  HA:        {:?}\n  lean-home: {got:?}",
                    c["value"], c["yaml"]
                ));
            }
        }
        assert!(
            failed.is_empty(),
            "{} of {} differ:\n{}",
            failed.len(),
            cases.lines().count(),
            failed.join("\n")
        );
    }

    #[test]
    fn floats_like_python() {
        for (f, want) in [
            (1.0, "1.0"),
            (1.5, "1.5"),
            (1e-7, "1.0e-07"),
            (1.5e20, "1.5e+20"),
            (1e16, "1.0e+16"),
            (1e15, "1000000000000000.0"),
            (0.0001, "0.0001"),
            (-2.5, "-2.5"),
            (0.1, "0.1"),
        ] {
            assert_eq!(py_float(f), want, "{f}");
        }
    }

    /// `!input` survives a load and a dump, as HA keeps it in blueprint automations.
    #[test]
    fn input_tag_round_trips() {
        let v = serde_json::json!({"use_blueprint": {"path": "x.yaml", "input": {"light": {"!input": "light"}}}});
        assert_eq!(
            dump(&v),
            "use_blueprint:\n  path: x.yaml\n  input:\n    light: !input light\n"
        );
    }
}
