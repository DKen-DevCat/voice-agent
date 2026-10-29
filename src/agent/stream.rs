//! The shapes of claude's stream-json messages, read leniently: a field of an
//! unexpected type reads as empty instead of spoiling the whole message.

use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};

fn lenient_string<'de, D: Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => s,
        _ => String::new(),
    })
}

fn lenient_bool<'de, D: Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)?.as_bool().unwrap_or(false))
}

fn lenient_i64<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    let v = Value::deserialize(d)?;
    Ok(v.as_i64().or_else(|| v.as_f64().map(|f| f as i64)).unwrap_or(0))
}

fn lenient_map<'de, D: Deserializer<'de>>(d: D) -> Result<Map<String, Value>, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::Object(m) => m,
        _ => Map::new(),
    })
}

fn lenient<'de, T: serde::de::DeserializeOwned, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    Ok(serde_json::from_value(Value::deserialize(d)?).ok())
}

#[derive(Deserialize, Default, Debug)]
pub struct StreamMsg {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub typ: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub subtype: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub session_id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub model: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub parent_tool_use_id: String,
    #[serde(default, deserialize_with = "lenient")]
    pub event: Option<StreamEvent>,
    #[serde(default, deserialize_with = "lenient")]
    pub message: Option<Message>,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_error: bool,
    #[serde(default, deserialize_with = "lenient_string")]
    pub result: String,
    /// The Agent tool's result says whether the subagent went to the background
    #[serde(default)]
    pub tool_use_result: Value,
}

#[derive(Deserialize, Default, Debug)]
pub struct StreamEvent {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub typ: String,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub index: i64,
    #[serde(default, deserialize_with = "lenient")]
    pub content_block: Option<Typed>,
    #[serde(default, deserialize_with = "lenient")]
    pub delta: Option<Delta>,
}

#[derive(Deserialize, Default, Debug)]
pub struct Typed {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub typ: String,
}

#[derive(Deserialize, Default, Debug)]
pub struct Delta {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub typ: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: String,
}

#[derive(Deserialize, Default, Debug)]
pub struct Message {
    /// A plain string (the user's own message) or a list of blocks
    #[serde(default)]
    pub content: Value,
    #[serde(default, deserialize_with = "lenient_string")]
    pub model: String,
    /// Assistant messages: the request's tokens
    #[serde(default, deserialize_with = "lenient")]
    pub usage: Option<TokenUsage>,
}

#[derive(Deserialize, Default, Debug, Clone, Copy)]
pub struct TokenUsage {
    #[serde(default, deserialize_with = "lenient_i64")]
    pub input_tokens: i64,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub output_tokens: i64,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub cache_read_input_tokens: i64,
    #[serde(default, deserialize_with = "lenient_i64")]
    pub cache_creation_input_tokens: i64,
}

impl TokenUsage {
    pub fn sum(&self) -> i64 {
        self.input_tokens + self.output_tokens + self.cache_read_input_tokens + self.cache_creation_input_tokens
    }
}

/// One block of an assistant or user message: text, a tool_use (id, name,
/// input) or a tool_result (tool_use_id, content).
#[derive(Deserialize, Default, Debug, Clone)]
pub struct ContentBlock {
    #[serde(rename = "type", default, deserialize_with = "lenient_string")]
    pub typ: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub id: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub name: String,
    #[serde(default, deserialize_with = "lenient_map")]
    pub input: Map<String, Value>,
    #[serde(default, deserialize_with = "lenient_string")]
    pub text: String,
    #[serde(default, deserialize_with = "lenient_string")]
    pub tool_use_id: String,
    #[serde(default)]
    pub content: Value,
    #[serde(default, deserialize_with = "lenient_bool")]
    pub is_error: bool,
}

impl StreamMsg {
    /// The blocks of an assistant or user message (none for a plain string).
    pub fn blocks(&self) -> Vec<ContentBlock> {
        match &self.message {
            Some(m) if self.typ == "assistant" || self.typ == "user" => match &m.content {
                Value::Array(items) => items.iter().filter_map(|v| serde_json::from_value(v.clone()).ok()).collect(),
                _ => Vec::new(),
            },
            _ => Vec::new(),
        }
    }
}

pub fn input_str(m: &Map<String, Value>, k: &str) -> String {
    m.get(k).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The text of a tool_result's content: a string, or text blocks.
pub fn block_text(raw: &Value) -> String {
    match raw {
        Value::String(s) => s.trim().to_string(),
        Value::Array(items) => {
            let parts: Vec<&str> = items
                .iter()
                .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .filter(|t| !t.is_empty())
                .collect();
            parts.join("\n").trim().to_string()
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn odd_shapes_do_not_spoil_a_message() {
        let m: StreamMsg = serde_json::from_str(
            r#"{"type":"result","result":{"not":"a string"},"is_error":null,"parent_tool_use_id":null,"message":"nope","session_id":"s1"}"#,
        )
        .unwrap();
        assert_eq!((m.typ.as_str(), m.result.as_str(), m.is_error, m.session_id.as_str()), ("result", "", false, "s1"));
        assert!(m.message.is_none() && m.parent_tool_use_id.is_empty());
    }

    #[test]
    fn blocks_of_a_message() {
        let m: StreamMsg = serde_json::from_str(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"T1","name":"Read","input":{"file_path":"/x"}}]}}"#,
        )
        .unwrap();
        let b = m.blocks();
        assert_eq!(b.len(), 2);
        assert_eq!(input_str(&b[1].input, "file_path"), "/x");
        let m: StreamMsg = serde_json::from_str(r#"{"type":"user","message":{"role":"user","content":"hello"}}"#).unwrap();
        assert!(m.blocks().is_empty());
        assert_eq!(block_text(&serde_json::json!([{"type":"text","text":"All green."}])), "All green.");
        assert_eq!(block_text(&serde_json::json!(" ok ")), "ok");
    }
}
