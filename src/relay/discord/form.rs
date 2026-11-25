//! The in-house reader for OMP's HTML forms (DISC-OUT req. 59-64; DESIGN 10.2): a strict parser of a
//! small allowlist of elements that either yields the form's fields or refuses the form, in which case
//! it is answered locally only. Refused: over 16 KiB, over 256 elements, nesting deeper than 24, an
//! element outside the allowlist, event-handler or style attributes, hidden-by-attribute or read-only
//! fields, duplicate attributes or IDs, unknown entities, malformed nesting, secret-looking fields.

use std::collections::HashSet;

const HTML_MAX: usize = 16 * 1024;
const ELEMENTS_MAX: usize = 256;
const DEPTH_MAX: usize = 24;
const FIELDS_MAX: usize = 16;
const OPTIONS_MAX: usize = 32;
const NAME_MAX: usize = 64;

/// Elements a form may use; the void ones never close.
const ALLOWED: [&str; 22] = [
    "form", "fieldset", "legend", "label", "input", "select", "option", "textarea", "div", "span",
    "p", "br", "hr", "b", "strong", "i", "em", "small", "ul", "ol", "li", "code",
];
const VOID: [&str; 3] = ["input", "br", "hr"];
const SECRET_WORDS: [&str; 10] = [
    "password",
    "passwd",
    "token",
    "apikey",
    "api_key",
    "api-key",
    "secret",
    "credential",
    "otp",
    "card",
];

/// One choice of a field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Choice {
    pub(crate) value: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
}

/// What a field takes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum FieldKind {
    /// Radio group or single select.
    One,
    /// Checkboxes or a multiple select.
    Many,
    /// Text, textarea or number.
    Text,
    Hidden,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Field {
    pub(crate) name: String,
    pub(crate) label: String,
    pub(crate) kind: FieldKind,
    pub(crate) choices: Vec<Choice>,
    /// Default text, or a hidden field's value.
    pub(crate) value: String,
    pub(crate) required: bool,
    pub(crate) limits: Limits,
}

/// What a text field accepts (DISC-OUT req. 63, 72): a number within `min`/`max`, or text within
/// `min_len`/`max_len`; `long` for a textarea.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Limits {
    pub(crate) number: bool,
    pub(crate) min: Option<String>,
    pub(crate) max: Option<String>,
    pub(crate) min_len: Option<usize>,
    pub(crate) max_len: Option<usize>,
    pub(crate) long: bool,
}

impl Limits {
    fn of(t: &Tag, number: bool, long: bool) -> Result<Self, String> {
        let num = |k: &str| -> Result<Option<String>, String> {
            match t.attr(k) {
                Some(v) if v.trim().parse::<f64>().is_ok_and(f64::is_finite) => {
                    Ok(Some(v.trim().to_string()))
                }
                Some(_) => Err(format!("a {k} that is not a number")),
                None => Ok(None),
            }
        };
        let len = |k: &str| -> Result<Option<usize>, String> {
            t.attr(k)
                .map(|v| {
                    v.trim()
                        .parse::<usize>()
                        .map_err(|_| format!("a {k} that is not a count"))
                })
                .transpose()
        };
        Ok(Self {
            number,
            min: if number { num("min")? } else { None },
            max: if number { num("max")? } else { None },
            min_len: len("minlength")?,
            max_len: len("maxlength")?,
            long,
        })
    }

    /// Checks one answer; the caller words the misfit in the owner's language.
    pub(crate) fn check(&self, value: &str) -> Result<(), Misfit> {
        let count = value.chars().count();
        if self.min_len.is_some_and(|m| count < m) || self.max_len.is_some_and(|m| count > m) {
            return Err(Misfit::Length);
        }
        if !self.number || value.is_empty() {
            return Ok(());
        }
        let n = value
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|n| n.is_finite())
            .ok_or(Misfit::NotNumber)?;
        let bound = |b: &Option<String>| b.as_deref().and_then(|v| v.parse::<f64>().ok());
        if bound(&self.min).is_some_and(|m| n < m) || bound(&self.max).is_some_and(|m| n > m) {
            return Err(Misfit::Range);
        }
        Ok(())
    }
}

/// How an answer misses a field's [`Limits`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Misfit {
    /// Fewer or more characters than `min_len` / `max_len` allow.
    Length,
    /// Not a number in a number field.
    NotNumber,
    /// A number outside `min` / `max`.
    Range,
}

/// A readable form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Form {
    /// Visible text outside the fields (legends of single fields included).
    pub(crate) text: String,
    pub(crate) fields: Vec<Field>,
}

impl Form {
    /// The editable fields.
    pub(crate) fn editable(&self) -> Vec<&Field> {
        self.fields
            .iter()
            .filter(|f| f.kind != FieldKind::Hidden)
            .collect()
    }
}

struct Tag {
    name: String,
    attrs: Vec<(String, String)>,
    close: bool,
}

impl Tag {
    fn attr(&self, key: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
    fn has(&self, key: &str) -> bool {
        self.attrs.iter().any(|(k, _)| k == key)
    }
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" | "#39" => '\'',
        "nbsp" => ' ',
        _ => {
            let n = name.strip_prefix('#')?;
            let code = match n.strip_prefix(['x', 'X']) {
                Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                None => n.parse().ok()?,
            };
            char::from_u32(code)?
        }
    })
}

fn decode(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(rest.get(..at).unwrap_or_default());
        let tail = rest.get(at + 1..).unwrap_or_default();
        let end = tail
            .find(';')
            .filter(|e| *e <= 10)
            .ok_or("an unknown entity")?;
        let c = entity(tail.get(..end).unwrap_or_default()).ok_or("an unknown entity")?;
        out.push(c);
        rest = tail.get(end + 1..).unwrap_or_default();
    }
    out.push_str(rest);
    Ok(out)
}

/// Parses one tag body (between `<` and `>`).
fn tag(body: &str) -> Result<Tag, String> {
    let (close, body) = match body.strip_prefix('/') {
        Some(b) => (true, b),
        None => (false, body),
    };
    let body = body.strip_suffix('/').unwrap_or(body).trim();
    let name_end = body.find(char::is_whitespace).unwrap_or(body.len());
    let name = body
        .get(..name_end)
        .unwrap_or_default()
        .to_ascii_lowercase();
    if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric()) {
        return Err("a malformed tag".into());
    }
    let mut attrs: Vec<(String, String)> = Vec::new();
    let mut rest = body.get(name_end..).unwrap_or_default().trim_start();
    while !rest.is_empty() {
        let key_end = rest
            .find(|c: char| c == '=' || c.is_whitespace())
            .unwrap_or(rest.len());
        let key = rest.get(..key_end).unwrap_or_default().to_ascii_lowercase();
        if key.is_empty()
            || !key
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err("a malformed attribute".into());
        }
        rest = rest.get(key_end..).unwrap_or_default().trim_start();
        let value = if let Some(r) = rest.strip_prefix('=') {
            let r = r.trim_start();
            let (v, after) = if let Some(q @ ('"' | '\'')) = r.chars().next() {
                let inner = r.get(1..).unwrap_or_default();
                let end = inner.find(q).ok_or("an unclosed attribute value")?;
                (
                    inner.get(..end).unwrap_or_default(),
                    inner.get(end + 1..).unwrap_or_default(),
                )
            } else {
                let end = r.find(char::is_whitespace).unwrap_or(r.len());
                (
                    r.get(..end).unwrap_or_default(),
                    r.get(end..).unwrap_or_default(),
                )
            };
            rest = after.trim_start();
            decode(v)?
        } else {
            String::new()
        };
        if key.starts_with("on") || key == "style" {
            return Err(format!("the attribute {key} is not allowed"));
        }
        if attrs.iter().any(|(k, _)| *k == key) {
            return Err("a duplicate attribute".into());
        }
        attrs.push((key, value));
    }
    Ok(Tag { name, attrs, close })
}

fn secret(name: &str, hint: Option<&str>) -> bool {
    let all = format!(
        "{} {}",
        name.to_ascii_lowercase(),
        hint.unwrap_or_default().to_ascii_lowercase()
    );
    SECRET_WORDS.iter().any(|w| all.contains(w))
        || all.contains("one-time-code")
        || all.contains("cc-")
}

fn valid_name(name: &str) -> bool {
    name.len() <= NAME_MAX
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The reader's state while it walks the HTML.
#[derive(Default)]
struct Reader {
    stack: Vec<String>,
    ids: HashSet<String>,
    elements: usize,
    text: String,
    fields: Vec<Field>,
    /// Text collected for the open label, legend, option or textarea.
    label: Option<String>,
    legend: Option<String>,
    option: Option<(String, bool, String)>,
    textarea: Option<(String, String, bool, Limits)>,
    /// The open select's field name.
    select: Option<String>,
    /// Fields (and choice) inside the open label, waiting for its text.
    pending: Vec<(usize, Option<usize>)>,
}

impl Reader {
    fn text(&mut self, words: &str) {
        if let Some((_, _, w)) = self.option.as_mut() {
            w.push_str(words);
        } else if let Some((_, w, _, _)) = self.textarea.as_mut() {
            w.push_str(words);
        } else if let Some(l) = self.legend.as_mut() {
            l.push_str(words);
        } else if let Some(l) = self.label.as_mut() {
            l.push_str(words);
        } else {
            self.text.push_str(words);
        }
    }

    fn close(&mut self, name: &str) -> Result<(), String> {
        if self.stack.last().map(String::as_str) != Some(name) {
            return Err("malformed nesting".into());
        }
        self.stack.pop();
        match name {
            "label" => {
                let words = self.label.take().unwrap_or_default().trim().to_string();
                for (f, c) in self.pending.drain(..) {
                    if let Some(field) = self.fields.get_mut(f) {
                        match c.and_then(|c| field.choices.get_mut(c)) {
                            Some(choice) => choice.label.clone_from(&words),
                            None => field.label.clone_from(&words),
                        }
                    }
                }
            }
            "legend" => {
                let words = self.legend.take().unwrap_or_default();
                self.text.push_str(words.trim());
                self.text.push('\n');
            }
            "option" => {
                let (value, selected, words) = self.option.take().ok_or("malformed nesting")?;
                let name = self.select.as_ref().ok_or("an option outside a select")?;
                let field = self
                    .fields
                    .iter_mut()
                    .find(|f| f.name == *name)
                    .ok_or("malformed select")?;
                let words = words.trim().to_string();
                let value = if value.is_empty() {
                    words.clone()
                } else {
                    value
                };
                field.choices.push(Choice {
                    value,
                    label: words,
                    selected,
                });
            }
            "select" => self.select = None,
            "textarea" => {
                let (name, words, required, limits) =
                    self.textarea.take().ok_or("malformed nesting")?;
                self.fields.push(Field {
                    label: String::new(),
                    name,
                    kind: FieldKind::Text,
                    choices: Vec::new(),
                    value: words,
                    required,
                    limits,
                });
            }
            _ => {}
        }
        Ok(())
    }

    fn open(&mut self, t: &Tag) -> Result<(), String> {
        self.elements += 1;
        if self.elements > ELEMENTS_MAX {
            return Err("over 256 elements".into());
        }
        if let Some(id) = t.attr("id")
            && !self.ids.insert(id.to_string())
        {
            return Err("a duplicate ID".into());
        }
        if t.has("hidden") || t.has("readonly") || t.has("disabled") && t.name != "option" {
            return Err("hidden, read-only or disabled elements are not allowed".into());
        }
        if !VOID.contains(&t.name.as_str()) {
            self.stack.push(t.name.clone());
            if self.stack.len() > DEPTH_MAX {
                return Err("nesting deeper than 24".into());
            }
        }
        match t.name.as_str() {
            "label" => self.label = Some(String::new()),
            "legend" => self.legend = Some(String::new()),
            "br" | "p" | "div" | "li" => self.text.push('\n'),
            "input" => self.input(t)?,
            "select" => {
                let name = field_name(t)?;
                self.new_field(&name)?;
                let multiple = t.has("multiple");
                self.fields.push(Field {
                    name: name.clone(),
                    label: String::new(),
                    kind: if multiple {
                        FieldKind::Many
                    } else {
                        FieldKind::One
                    },
                    choices: Vec::new(),
                    value: String::new(),
                    required: t.has("required"),
                    limits: Limits::default(),
                });
                if self.label.is_some() {
                    self.pending.push((self.fields.len() - 1, None));
                }
                self.select = Some(name);
            }
            "option" => {
                if self.select.is_none() {
                    return Err("an option outside a select".into());
                }
                let value = t.attr("value").unwrap_or_default().to_string();
                self.option = Some((value, t.has("selected"), String::new()));
            }
            "textarea" => {
                let name = field_name(t)?;
                self.new_field(&name)?;
                let limits = Limits::of(t, false, true)?;
                self.textarea = Some((name, String::new(), t.has("required"), limits));
            }
            _ => {}
        }
        Ok(())
    }

    fn new_field(&self, name: &str) -> Result<(), String> {
        if self.fields.iter().any(|f| f.name == name) {
            return Err("a field name used twice".into());
        }
        Ok(())
    }

    fn input(&mut self, t: &Tag) -> Result<(), String> {
        let kind = t.attr("type").unwrap_or("text").to_ascii_lowercase();
        if kind == "password" {
            return Err("a field that looks like a secret".into());
        }
        let name = field_name(t)?;
        let value = t.attr("value").unwrap_or_default().to_string();
        let required = t.has("required");
        match kind.as_str() {
            "radio" | "checkbox" => {
                let field_kind = if kind == "radio" {
                    FieldKind::One
                } else {
                    FieldKind::Many
                };
                let at = if let Some(i) = self.fields.iter().position(|f| f.name == name) {
                    i
                } else {
                    self.fields.push(Field {
                        name,
                        label: legend_text(&self.text),
                        kind: field_kind.clone(),
                        choices: Vec::new(),
                        value: String::new(),
                        required,
                        limits: Limits::default(),
                    });
                    self.fields.len() - 1
                };
                let field = self.fields.get_mut(at).ok_or("malformed form")?;
                if field.kind != field_kind {
                    return Err("a field name used twice".into());
                }
                if field.choices.iter().any(|c| c.value == value) {
                    return Err("option values are not unique".into());
                }
                field.choices.push(Choice {
                    label: value.clone(),
                    value,
                    selected: t.has("checked"),
                });
                let choice = field.choices.len() - 1;
                if self.label.is_some() {
                    self.pending.push((at, Some(choice)));
                }
            }
            "text" | "number" | "hidden" => {
                self.new_field(&name)?;
                self.fields.push(Field {
                    name,
                    label: String::new(),
                    kind: if kind == "hidden" {
                        FieldKind::Hidden
                    } else {
                        FieldKind::Text
                    },
                    choices: Vec::new(),
                    value,
                    required,
                    limits: Limits::of(t, kind == "number", false)?,
                });
                if self.label.is_some() {
                    self.pending.push((self.fields.len() - 1, None));
                }
            }
            other => return Err(format!("the input type {other} is not supported")),
        }
        Ok(())
    }
}

/// A field's name, checked: a letter first, then letters, digits, `_` or `-`, and nothing that
/// looks like a secret.
fn field_name(t: &Tag) -> Result<String, String> {
    let name = t.attr("name").ok_or("a field without a name")?;
    if !valid_name(name) {
        return Err("a field name that is not allowed".into());
    }
    if secret(name, t.attr("autocomplete")) {
        return Err("a field that looks like a secret".into());
    }
    Ok(name.to_string())
}

/// Reads a form, or says why it stays local.
pub(crate) fn read(html: &str) -> Result<Form, String> {
    if html.len() > HTML_MAX {
        return Err("the form is over 16 KiB".into());
    }
    let mut r = Reader::default();
    let mut rest = html;
    while !rest.is_empty() {
        if rest.starts_with('<') {
            if rest.starts_with("<!--") {
                return Err("comments are not allowed".into());
            }
            let end = rest.find('>').ok_or("an unclosed tag")?;
            let t = tag(rest.get(1..end).unwrap_or_default())?;
            rest = rest.get(end + 1..).unwrap_or_default();
            if !ALLOWED.contains(&t.name.as_str()) {
                return Err(format!("the element <{}> is not allowed", t.name));
            }
            if t.close {
                r.close(&t.name)?;
            } else {
                r.open(&t)?;
            }
            continue;
        }
        let at = rest.find('<').unwrap_or(rest.len());
        r.text(&decode(rest.get(..at).unwrap_or_default())?);
        rest = rest.get(at..).unwrap_or_default();
    }
    if !r.stack.is_empty() {
        return Err("malformed nesting".into());
    }
    if r.fields.len() > FIELDS_MAX || r.fields.iter().any(|f| f.choices.len() > OPTIONS_MAX) {
        return Err("too many fields or options".into());
    }
    let form = Form {
        text: r
            .text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .collect::<Vec<_>>()
            .join("\n"),
        fields: r.fields,
    };
    if form.editable().is_empty() {
        return Err("no editable field".into());
    }
    Ok(form)
}

/// The legend text read so far, as a group's label.
fn legend_text(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_omp_ask_forms_and_refuses_the_rest() {
        let html = r#"<form><fieldset><legend>Pick a color</legend><label><input type="radio" name="color" value="Red"> Red</label><label><input type="radio" name="color" value="Blue" checked> Blue</label></fieldset></form>"#;
        let f = read(html).unwrap();
        assert_eq!(f.text, "Pick a color");
        let [field] = f.editable()[..] else { panic!() };
        assert_eq!(field.kind, FieldKind::One);
        assert_eq!(field.label, "Pick a color");
        assert_eq!(
            field.choices[1],
            Choice {
                value: "Blue".into(),
                label: "Blue".into(),
                selected: true
            }
        );
        let select = r#"<form><label>Size <select name="size"><option value="s">Small &amp; light</option><option value="l" selected>Large</option></select></label><input type="hidden" name="k" value="v"></form>"#;
        let f = read(select).unwrap();
        assert_eq!(f.fields[0].choices[0].label, "Small & light");
        assert_eq!(f.fields[1].kind, FieldKind::Hidden);
        for bad in [
            r#"<form><input type="password" name="p"></form>"#,
            r#"<form><input name="api_token"></form>"#,
            "<form><script>x</script></form>",
            r#"<form onclick="x"><input name="a"></form>"#,
            r#"<form><div><input name="a"></form></div>"#,
            r#"<form><input name="a" readonly></form>"#,
            r#"<form>&bogus;<input name="a"></form>"#,
            r#"<form><p id="x"></p><p id="x"></p><input name="a"></form>"#,
            r#"<form><input name="a" name="b"></form>"#,
            "<form><p>only text</p></form>",
        ] {
            assert!(read(bad).is_err(), "{bad}");
        }
        assert!(read(&"<p>x</p>".repeat(3000)).is_err());
    }
}
