//! The voice tools (VOICE-14, VOICE req. 68-71): the provider's hosted web search plus three
//! functions the hub runs. No tool runs a shell, writes outside the hub's state, or logs its
//! arguments.

use serde_json::{Value, json};

use crate::host::StatusPart;

/// Lane status is cut to this many characters.
pub const LANES_MAX: usize = 3000;
/// Each report is cut to this many characters.
pub const REPORT_MAX: usize = 350;

/// The backend's tool list.
pub(crate) fn definitions() -> Value {
    json!([
        {"type": "web_search"},
        {
            "type": "function",
            "name": "read_status",
            "description": "Read the live state of the lanes and their recent reports. Set \
                early_update to get a short partial status spoken while other work is pending.",
            "parameters": {
                "type": "object",
                "properties": {
                    "parts": {"type": "array", "minItems": 1,
                        "items": {"type": "string", "enum": ["lanes", "reports"]}},
                    "early_update": {"type": "boolean"}
                },
                "required": ["parts", "early_update"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "leave_note",
            "description": "Post a short written note to the owner's text channel.",
            "parameters": {
                "type": "object",
                "properties": {"text": {"type": "string"}},
                "required": ["text"],
                "additionalProperties": false
            }
        },
        {
            "type": "function",
            "name": "pass_to_lead",
            "description": "Hand a request to the lead agent. The result says whether the lead \
                received it; only 'delivered' means it did.",
            "parameters": {
                "type": "object",
                "properties": {
                    "request": {"type": "string"},
                    "high_risk": {"type": "boolean"}
                },
                "required": ["request", "high_risk"],
                "additionalProperties": false
            }
        }
    ])
}

/// A parsed call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Call {
    Status { parts: Vec<StatusPart>, early: bool },
    Note(String),
    Pass { request: String, high_risk: bool },
}

impl Call {
    /// Status reads run at once; the others run one at a time (VOICE req. 66).
    pub(crate) fn is_status(&self) -> bool {
        matches!(self, Self::Status { .. })
    }

    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::Status { .. } => "status read",
            Self::Note(_) => "note",
            Self::Pass { .. } => "pass to lead",
        }
    }
}

/// Parses a call; the error is the explanatory failure returned to the backend.
pub(crate) fn parse(name: &str, arguments: &str) -> Result<Call, String> {
    let args: Value =
        serde_json::from_str(arguments).map_err(|_| "arguments are not JSON".to_string())?;
    let Some(obj) = args.as_object() else {
        return Err("arguments must be an object".into());
    };
    match name {
        "read_status" => {
            let Some(list) = obj.get("parts").and_then(Value::as_array) else {
                return Err("parts must be a list".into());
            };
            let mut parts = Vec::new();
            for p in list {
                let part = match p.as_str() {
                    Some("lanes") => StatusPart::Lanes,
                    Some("reports") => StatusPart::Reports,
                    _ => return Err("parts may contain only lanes and reports".into()),
                };
                if !parts.contains(&part) {
                    parts.push(part);
                }
            }
            if parts.is_empty() {
                return Err("parts must not be empty".into());
            }
            let early = match obj.get("early_update") {
                None => false,
                Some(Value::Bool(b)) => *b,
                Some(_) => return Err("early_update must be true or false".into()),
            };
            if obj.keys().any(|k| k != "parts" && k != "early_update") {
                return Err("unknown argument".into());
            }
            Ok(Call::Status { parts, early })
        }
        "leave_note" => {
            let text = obj.get("text").and_then(Value::as_str).unwrap_or("");
            crate::text::clean_note(text)
                .map(Call::Note)
                .ok_or_else(|| "the note is empty".into())
        }
        "pass_to_lead" => {
            let text = obj.get("request").and_then(Value::as_str).unwrap_or("");
            let high_risk = obj
                .get("high_risk")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            crate::text::clean_request(text)
                .map(|request| Call::Pass { request, high_risk })
                .ok_or_else(|| "the request is empty".into())
        }
        _ => Err("unknown tool".into()),
    }
}

/// Cuts to at most `max` characters.
pub(crate) fn cut(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_arguments() {
        assert_eq!(
            parse(
                "read_status",
                r#"{"parts":["lanes","lanes"],"early_update":true}"#
            ),
            Ok(Call::Status {
                parts: vec![StatusPart::Lanes],
                early: true
            })
        );
        assert!(parse("read_status", r#"{"parts":[]}"#).is_err());
        assert!(parse("read_status", r#"{"parts":["shell"]}"#).is_err());
        assert!(parse("read_status", r#"["lanes"]"#).is_err());
        assert!(parse("leave_note", r#"{"text":"  "}"#).is_err());
        assert!(parse("run_shell", "{}").is_err());
        assert_eq!(
            parse(
                "pass_to_lead",
                r#"{"request":"check `ci`","high_risk":false}"#
            ),
            Ok(Call::Pass {
                request: "check ci".into(),
                high_risk: false
            })
        );
    }
}
