//! Model providers.
//!
//! 1. **Muse Spark (primary, server-hosted)** — the live client is
//!    `MuseSparkProvider`: OpenAI-compatible Chat Completions against
//!    Meta's Model API at `https://api.meta.ai/v1` (model
//!    `muse-spark-1.3`, or `muse-spark-1.3-contributor` via
//!    `MUSE_SPARK_MODEL`), Bearer `MODEL_API_KEY`. The model itself
//!    is NOT extracted or run locally — it runs on Meta's servers and
//!    this VM is only the agent loop + tools. The key is still pending
//!    (Meta application is pending on Peter's own device; connector
//!    `custom.meta` does not exist yet), so without a key the live
//!    provider reports NOT_CONFIGURED instead of faking a call.
//!    Until the key lands, Muse Spark is also available in-session,
//!    exactly as in `~/workspace/model-compare/muse_spark.py`:
//!    (a) `BankProvider` replays answers authored in this session,
//!    keyed by SHA1 of the canonical (messages, tools) JSON — the same
//!    key scheme as muse_spark.py, truncated to 16 hex; (b)
//!    `InteractiveProvider` prints the conversation + tools and reads
//!    the model's turn from stdin, so the live agent (Muse Spark
//!    itself) drives the loop turn by turn. Both are honest: the bank
//!    is labelled self-test, never a live API claim.
//! 2. **DeepSeek backup** — OpenAI-compatible client for
//!    `https://api.deepseek.com/chat/completions`, model
//!    `deepseek-flash` (DeepSeek V4.1 Flash, current per the provider's
//!    model list; `deepseek-chat`/`deepseek-reasoner` are retired
//!    aliases). Credential: `hsurr:` surrogate from authd, swapped
//!    for the real key only on egress to api.deepseek.com, same
//!    mechanism as the Python/ Rust Anthropic harnesses. Note: the
//!    egress block on api.deepseek.com recorded in the DeepSeek
//!    connector doc NO LONGER HELD as of 2026-11-02 (live DeepSeek
//!    calls succeeded that day) — re-check the doc before relying on
//!    it, and treat a live failure as a real failure, not a fake
//!    success. The client itself is fully unit-tested on canned
//!    responses.

use crate::agent::{Model, ModelTurn, Msg, ToolCall, ToolDef};
use serde::{Deserialize, Serialize};
use sha1::{Digest, Sha1};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

// ---------- Muse Spark bank ----------
pub fn bank_key(messages: &[Msg], tools: &[ToolDef]) -> String {
    let canon = serde_json::json!({
        "m": messages,
        "t": tools,
        "tag": serde_json::Value::Null,
    });
    let s = serde_json::to_string(&canon).unwrap_or_default();
    let mut h = Sha1::new();
    h.update(s.as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

/// Parse authored turn text into (clean_text, tool_calls).
/// Authored format mirrors muse_spark.py: ```tool JSON blocks.
pub fn parse_authored(text: &str, key: &str) -> (String, Vec<ToolCall>) {
    let mut calls = vec![];
    let mut clean = text.to_string();
    let mut idx = 0;
    while let Some(start) = clean[idx..].find("```tool") {
        let abs = idx + start;
        let body_start = match clean[abs..].find('\n') {
            Some(n) => abs + n + 1,
            None => break,
        };
        let end = match clean[body_start..].find("```") {
            Some(n) => body_start + n,
            None => break,
        };
        let body = &clean[body_start..end];
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
            let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("").to_string();
            let args = v.get("arguments").cloned().unwrap_or(serde_json::json!({}));
            if !name.is_empty() {
                calls.push(ToolCall { id: format!("muse-{}-{}", &key[..8.min(key.len())], calls.len()), name, arguments: args });
            }
        }
        // Remove the block from clean text.
        clean = format!("{}{}", &clean[..abs], &clean[end + 3..]);
        idx = abs;
    }
    (clean.trim().to_string(), calls)
}

pub struct BankProvider {
    pub name: String,
    pub bank: HashMap<String, String>,
}
impl BankProvider {
    pub fn load(path: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("bank read: {e}"))?;
        let raw: HashMap<String, serde_json::Value> =
            serde_json::from_str(&text).map_err(|e| format!("bank json: {e}"))?;
        let mut bank = HashMap::new();
        for (k, v) in raw {
            let t = v.get("text").and_then(|x| x.as_str()).map(|s| s.to_string()).unwrap_or_else(|| v.as_str().unwrap_or("").to_string());
            bank.insert(k, t);
        }
        Ok(BankProvider { name: "muse-spark-bank".into(), bank })
    }
}
impl Model for BankProvider {
    fn chat(&mut self, messages: &[Msg], tools: &[ToolDef]) -> Result<ModelTurn, String> {
        let key = bank_key(messages, tools);
        let authored = self.bank.get(&key).ok_or_else(|| format!("muse-spark bank has no answer for key {key}; author it in-session first"))?;
        let (text, calls) = parse_authored(authored, &key);
        Ok(ModelTurn { text, tool_calls: calls })
    }
    fn name(&self) -> &str {
        &self.name
    }
}

/// Interactive Muse Spark provider: prints the turn, reads authored
/// text from stdin until a line containing only <<<END>>>.
pub struct InteractiveProvider {
    pub name: String,
}
impl InteractiveProvider {
    pub fn new() -> Self {
        InteractiveProvider { name: "muse-spark-interactive".into() }
    }
}
impl Model for InteractiveProvider {
    fn chat(&mut self, messages: &[Msg], tools: &[ToolDef]) -> Result<ModelTurn, String> {
        println!("\n================ MUSE SPARK TURN ================");
        println!("{}", serde_json::to_string_pretty(messages).unwrap_or_default());
        println!("---- tools ----");
        println!("{}", serde_json::to_string_pretty(tools).unwrap_or_default());
        println!("---- write your response below; end with a line containing only <<<END>>> ----");
        std::io::stdout().flush().ok();
        let stdin = std::io::stdin();
        let mut lines = vec![];
        for line in stdin.lock().lines() {
            let line = line.map_err(|e| e.to_string())?;
            if line.trim() == "<<<END>>>" {
                break;
            }
            lines.push(line);
        }
        let authored = lines.join("\n");
        let key = bank_key(messages, tools);
        let (text, calls) = parse_authored(&authored, &key);
        Ok(ModelTurn { text, tool_calls: calls })
    }
    fn name(&self) -> &str {
        &self.name
    }
}

// ---------- DeepSeek ----------
pub const DEEPSEEK_BASE: &str = "https://api.deepseek.com";
pub const DEEPSEEK_FLASH_MODEL: &str = "deepseek-flash"; // DeepSeek V4.1 Flash
const AUTHD_SOCK: &str = "/run/hatch/auth/authd.sock";

pub fn surrogate_for(connector: &str) -> Result<String, String> {
    let mut stream = UnixStream::connect(AUTHD_SOCK).map_err(|e| format!("authd connect: {e}"))?;
    let body = format!(r#"{{"name":"{connector}"}}"#);
    let req = format!(
        "POST /v1/credentials/surrogate HTTP/1.1\r\nHost: authd.local\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(req.as_bytes()).map_err(|e| format!("authd write: {e}"))?;
    let mut raw = Vec::new();
    use std::io::Read;
    stream.read_to_end(&mut raw).map_err(|e| format!("authd read: {e}"))?;
    let text = String::from_utf8_lossy(&raw);
    let body = text.split("\r\n\r\n").nth(1).unwrap_or("");
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| format!("authd json: {e} body={body}"))?;
    for c in v.get("credentials").and_then(|x| x.as_array()).ok_or("no credentials")? {
        if c.get("name").and_then(|x| x.as_str()) == Some("access_token") {
            let s = c.get("surrogate").and_then(|x| x.as_str()).ok_or("no surrogate")?;
            if s.starts_with("hsurr:") {
                return Ok(s.to_string());
            }
            return Err("authd returned a non-surrogate value".into());
        }
    }
    Err("access_token surrogate not found".into())
}

pub fn deepseek_surrogate() -> Result<String, String> {
    surrogate_for("custom.deepseek")
}

/// Parse an OpenAI-compatible chat-completions response into a ModelTurn.
/// Pure function, unit-tested on canned JSON (no network needed).
pub fn parse_openai_response(body: &serde_json::Value) -> Result<ModelTurn, String> {
    let choice = body
        .get("choices")
        .and_then(|c| c.as_array())
        .and_then(|a| a.first())
        .ok_or("no choices")?;
    let msg = choice.get("message").ok_or("no message")?;
    let text = msg.get("content").and_then(|c| c.as_str()).unwrap_or("").to_string();
    let mut calls = vec![];
    if let Some(tcs) = msg.get("tool_calls").and_then(|t| t.as_array()) {
        for tc in tcs {
            let name = tc
                .get("function")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let args_raw = tc
                .get("function")
                .and_then(|f| f.get("arguments"))
                .and_then(|a| a.as_str())
                .unwrap_or("{}");
            let args: serde_json::Value = serde_json::from_str(args_raw).unwrap_or(serde_json::json!({}));
            calls.push(ToolCall {
                id: tc.get("id").and_then(|i| i.as_str()).unwrap_or("call-0").to_string(),
                name,
                arguments: args,
            });
        }
    }
    Ok(ModelTurn { text, tool_calls: calls })
}

pub fn openai_request_body(model: &str, messages: &[Msg], tools: &[ToolDef]) -> serde_json::Value {
    let msgs: Vec<serde_json::Value> = messages
        .iter()
        .map(|m| {
            let mut o = serde_json::json!({"role": m.role, "content": m.content});
            if let Some(tcs) = &m.tool_calls {
                o["tool_calls"] = serde_json::json!(tcs.iter().map(|tc| serde_json::json!({
                    "id": tc.id, "type": "function",
                    "function": {"name": tc.name, "arguments": tc.arguments.to_string()}
                })).collect::<Vec<_>>());
            }
            if let Some(id) = &m.tool_call_id {
                o["tool_call_id"] = serde_json::json!(id);
            }
            o
        })
        .collect();
    serde_json::json!({
        "model": model,
        "messages": msgs,
        "tools": tools.iter().map(|t| serde_json::json!({
            "type": "function",
            "function": {"name": t.name, "description": t.description, "parameters": t.parameters}
        })).collect::<Vec<_>>(),
        "tool_choice": "auto",
    })
}

pub struct DeepSeekProvider {
    pub name: String,
    pub model: String,
    pub surrogate: String,
    pub base: String,
}
impl DeepSeekProvider {
    pub fn new(surrogate: String, model: &str) -> Self {
        DeepSeekProvider { name: format!("deepseek:{model}"), model: model.to_string(), surrogate, base: DEEPSEEK_BASE.to_string() }
    }
    pub fn models(&self) -> Result<serde_json::Value, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("client: {e}"))?;
        let resp = client
            .get(format!("{}/models", self.base))
            .header("Authorization", format!("Bearer {}", self.surrogate))
            .send()
            .map_err(|e| format!("deepseek models request: {e}"))?;
        let status = resp.status();
        let text = resp.text().map_err(|e| format!("read: {e}"))?;
        if !status.is_success() {
            return Err(format!("deepseek models HTTP {status}: {}", &text[..text.len().min(300)]));
        }
        serde_json::from_str(&text).map_err(|e| format!("models json: {e}"))
    }
}
impl Model for DeepSeekProvider {
    fn chat(&mut self, messages: &[Msg], tools: &[ToolDef]) -> Result<ModelTurn, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(90))
            .build()
            .map_err(|e| format!("client: {e}"))?;
        let body = openai_request_body(&self.model, messages, tools);
        let mut last_err = String::new();
        for _attempt in 0..2 {
            match client
                .post(format!("{}/chat/completions", self.base))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.surrogate))
                .json(&body)
                .send()
            {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().map_err(|e| format!("read: {e}"))?;
                    if !status.is_success() {
                        return Err(format!("deepseek HTTP {status}: {}", &text[..text.len().min(500)]));
                    }
                    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("deepseek json: {e}"))?;
                    return parse_openai_response(&v);
                }
                Err(e) => {
                    last_err = format!("deepseek request: {e}");
                }
            }
        }
        Err(last_err)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

// ---------- Muse Spark (live, server-hosted) ----------
// Meta Model API: OpenAI-compatible Chat Completions at
// https://api.meta.ai/v1/chat/completions, model muse-spark-1.3.
// The model runs on Meta's servers; this provider only sends the
// agent-loop messages + tool definitions and parses the reply with
// the same OpenAI-compatible parser as DeepSeek. It never extracts,
// downloads, or runs model weights locally, and it never logs the
// key: errors name the source (env / custom.meta) only.
// Credential resolution, in order:
//   1. env MODEL_API_KEY, then META_API_KEY, then MUSE_SPARK_API_KEY
//      (a raw key is used transiently for this process only);
//   2. authd surrogate for connector custom.meta, once Peter hands
//      the key over via Secure Vault (same mechanism as DeepSeek).
// Without either, resolution fails with NOT_CONFIGURED — the worker
// then reports "error", it does not fall back silently to the bank.
pub const MUSE_SPARK_BASE: &str = "https://api.meta.ai/v1";
pub const MUSE_SPARK_MODEL: &str = "muse-spark-1.3";
/// Contributor tier id, selectable via MUSE_SPARK_MODEL (kept as a
/// named constant for callers; not the default).
#[allow(dead_code)]
pub const MUSE_SPARK_MODEL_CONTRIBUTOR: &str = "muse-spark-1.3-contributor";

pub fn muse_spark_base() -> String {
    std::env::var("META_API_BASE")
        .or_else(|_| std::env::var("MUSE_SPARK_API_BASE"))
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| MUSE_SPARK_BASE.to_string())
}

pub fn muse_spark_model() -> String {
    std::env::var("MUSE_SPARK_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| MUSE_SPARK_MODEL.to_string())
}

pub fn muse_spark_key_from_env() -> Option<String> {
    for var in ["MODEL_API_KEY", "META_API_KEY", "MUSE_SPARK_API_KEY"] {
        if let Ok(v) = std::env::var(var) {
            if !v.trim().is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// Resolve the Muse Spark credential, or a NOT_CONFIGURED error that
/// never contains the key value.
pub fn muse_spark_key() -> Result<String, String> {
    if let Some(k) = muse_spark_key_from_env() {
        return Ok(k);
    }
    match surrogate_for("custom.meta") {
        Ok(s) => Ok(s),
        Err(e) => Err(format!(
            "NOT_CONFIGURED: no Muse Spark key in MODEL_API_KEY/META_API_KEY/MUSE_SPARK_API_KEY and no custom.meta connector (authd: {e}). Peter: complete the Meta Model API application on your own device, then hand the key over via Secure Vault as connector custom.meta — do not paste it in chat."
        )),
    }
}

pub struct MuseSparkProvider {
    pub name: String,
    pub model: String,
    pub key: String,
    pub base: String,
}
impl MuseSparkProvider {
    pub fn new(key: String, model: &str) -> Self {
        MuseSparkProvider {
            name: format!("muse-spark:{model}"),
            model: model.to_string(),
            key,
            base: muse_spark_base(),
        }
    }
    /// Build from the normal credential resolution; fails NOT_CONFIGURED.
    pub fn from_env_or_vault() -> Result<Self, String> {
        let key = muse_spark_key()?;
        Ok(Self::new(key, &muse_spark_model()))
    }
    pub fn models(&self) -> Result<serde_json::Value, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| format!("client: {e}"))?;
        let resp = client
            .get(format!("{}/models", self.base))
            .header("Authorization", format!("Bearer {}", self.key))
            .send()
            .map_err(|e| format!("muse-spark models request: {e}"))?;
        let status = resp.status();
        let text = resp.text().map_err(|e| format!("read: {e}"))?;
        if !status.is_success() {
            return Err(format!("muse-spark models HTTP {status}: {}", &text[..text.len().min(300)]));
        }
        serde_json::from_str(&text).map_err(|e| format!("models json: {e}"))
    }
}
impl Model for MuseSparkProvider {
    fn chat(&mut self, messages: &[Msg], tools: &[ToolDef]) -> Result<ModelTurn, String> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(180))
            .build()
            .map_err(|e| format!("client: {e}"))?;
        let body = openai_request_body(&self.model, messages, tools);
        let mut last_err = String::new();
        for _attempt in 0..2 {
            match client
                .post(format!("{}/chat/completions", self.base))
                .header("Content-Type", "application/json")
                .header("Authorization", format!("Bearer {}", self.key))
                .json(&body)
                .send()
            {
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().map_err(|e| format!("read: {e}"))?;
                    if !status.is_success() {
                        return Err(format!("muse-spark HTTP {status}: {}", &text[..text.len().min(500)]));
                    }
                    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("muse-spark json: {e}"))?;
                    return parse_openai_response(&v);
                }
                Err(e) => {
                    last_err = format!("muse-spark request: {e}");
                }
            }
        }
        Err(last_err)
    }
    fn name(&self) -> &str {
        &self.name
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BankEntry {
    pub text: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::ToolDef;

    #[test]
    fn bank_key_stable_and_16_hex() {
        let msgs = vec![Msg { role: "user".into(), content: "hi".into(), tool_calls: None, tool_call_id: None }];
        let tools: Vec<ToolDef> = vec![];
        let k1 = bank_key(&msgs, &tools);
        let k2 = bank_key(&msgs, &tools);
        assert_eq!(k1, k2);
        assert_eq!(k1.len(), 16);
        assert!(k1.chars().all(|c| c.is_ascii_hexdigit()));
    }
    #[test]
    fn parse_authored_extracts_tool_and_cleans_text() {
        let text = "Fixing now.\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"README.md\"}}\n```\nDone.";
        let (clean, calls) = parse_authored(text, "abcdef0123456789");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert!(!clean.contains("```tool"));
        assert!(clean.contains("Fixing now"));
    }
    #[test]
    fn parse_openai_response_with_tool_call() {
        let v = serde_json::json!({
            "choices": [{"message": {
                "content": "reading",
                "tool_calls": [{"id": "call_1", "type": "function",
                    "function": {"name": "read_file", "arguments": "{\"path\": \"x.txt\"}"}}]
            }}]
        });
        let t = parse_openai_response(&v).unwrap();
        assert_eq!(t.text, "reading");
        assert_eq!(t.tool_calls.len(), 1);
        assert_eq!(t.tool_calls[0].name, "read_file");
        assert_eq!(t.tool_calls[0].arguments["path"], "x.txt");
    }
    #[test]
    fn parse_openai_response_no_choices_is_error() {
        assert!(parse_openai_response(&serde_json::json!({})).is_err());
    }
    #[test]
    fn openai_body_includes_tools_and_model() {
        let msgs = vec![Msg { role: "user".into(), content: "hi".into(), tool_calls: None, tool_call_id: None }];
        let tools = vec![ToolDef { name: "exec".into(), description: "run".into(), parameters: serde_json::json!({}) }];
        let b = openai_request_body("deepseek-flash", &msgs, &tools);
        assert_eq!(b["model"], "deepseek-flash");
        assert_eq!(b["tools"][0]["function"]["name"], "exec");
    }
    #[test]
    fn muse_spark_live_endpoint_and_model() {
        // Server-hosted, not local: the client must point at Meta's API
        // with the 1.3 model id, and the request body must carry it.
        assert_eq!(MUSE_SPARK_BASE, "https://api.meta.ai/v1");
        assert_eq!(MUSE_SPARK_MODEL, "muse-spark-1.3");
        let msgs = vec![Msg { role: "user".into(), content: "hi".into(), tool_calls: None, tool_call_id: None }];
        let b = openai_request_body(MUSE_SPARK_MODEL, &msgs, &[]);
        assert_eq!(b["model"], "muse-spark-1.3");
        let p = MuseSparkProvider::new("test-key-not-a-real-key".into(), MUSE_SPARK_MODEL);
        assert_eq!(p.name, "muse-spark:muse-spark-1.3");
        assert_eq!(p.base, MUSE_SPARK_BASE);
    }
}
