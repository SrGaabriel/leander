#![allow(clippy::cast_possible_truncation)]

use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct Documents {
    inner: Arc<RwLock<HashMap<String, Document>>>,
}

#[derive(Clone)]
struct Document {
    text: String,
}

impl Documents {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn handle_message(&self, body: &[u8]) {
        let Ok(v) = serde_json::from_slice::<Value>(body) else {
            return;
        };
        let Some(method) = v.get("method").and_then(Value::as_str) else {
            return;
        };
        let Some(params) = v.get("params") else {
            return;
        };
        match method {
            "textDocument/didOpen" => self.do_open(params).await,
            "textDocument/didChange" => self.do_change(params).await,
            "textDocument/didClose" => self.do_close(params).await,
            _ => {}
        }
    }

    pub async fn line_length_utf16(&self, uri: &str, line: u64) -> Option<u64> {
        let docs = self.inner.read().await;
        let doc = docs.get(uri)?;
        line_length_utf16(&doc.text, line)
    }

    pub async fn line_text(&self, uri: &str, line: u64) -> Option<String> {
        let docs = self.inner.read().await;
        let doc = docs.get(uri)?;
        doc.text
            .split('\n')
            .nth(line as usize)
            .map(|l| l.trim_end_matches('\r').to_string())
    }

    pub async fn full_text(&self, uri: &str) -> Option<String> {
        let docs = self.inner.read().await;
        docs.get(uri).map(|d| d.text.clone())
    }

    pub async fn line_count(&self, uri: &str) -> Option<u64> {
        let docs = self.inner.read().await;
        docs.get(uri)
            .map(|d| (d.text.split('\n').count() as u64).max(1))
    }

    async fn do_open(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let text = params
            .pointer("/textDocument/text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        self.inner
            .write()
            .await
            .insert(uri.to_string(), Document { text });
    }

    async fn do_change(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let Some(changes) = params.get("contentChanges").and_then(Value::as_array) else {
            return;
        };
        let mut docs = self.inner.write().await;
        let Some(doc) = docs.get_mut(uri) else { return };
        for change in changes {
            apply_change(&mut doc.text, change);
        }
    }

    async fn do_close(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        self.inner.write().await.remove(uri);
    }
}

fn line_length_utf16(text: &str, line: u64) -> Option<u64> {
    text.split('\n')
        .nth(line as usize)
        .map(|l| l.trim_end_matches('\r').encode_utf16().count() as u64)
}

fn apply_change(text: &mut String, change: &Value) {
    let new_text = change.get("text").and_then(Value::as_str).unwrap_or("");
    match change.get("range") {
        None => {
            *text = new_text.to_string();
        }
        Some(range) => {
            let start = position_to_byte_offset(text, range.get("start"));
            let end = position_to_byte_offset(text, range.get("end"));
            if let (Some(start), Some(end)) = (start, end)
                && start <= end
                && end <= text.len()
            {
                text.replace_range(start..end, new_text);
            }
        }
    }
}

fn position_to_byte_offset(text: &str, pos: Option<&Value>) -> Option<usize> {
    let pos = pos?;
    let target_line = pos.get("line")?.as_u64()? as usize;
    let target_char = pos.get("character")?.as_u64()? as usize;

    let mut line = 0usize;
    let mut byte = 0usize;
    let mut iter = text.char_indices();

    while line < target_line {
        match iter.next() {
            None => return Some(text.len()),
            Some((i, '\n')) => {
                byte = i + 1;
                line += 1;
            }
            Some(_) => {}
        }
    }

    let mut utf16 = 0usize;
    while utf16 < target_char {
        let mut buf = [0u16; 2];
        match iter.next() {
            None => return Some(text.len()),
            Some((i, '\n')) => return Some(i),
            Some((i, c)) => {
                let units = c.encode_utf16(&mut buf).len();
                utf16 += units;
                byte = i + c.len_utf8();
                if utf16 > target_char {
                    return Some(i);
                }
            }
        }
    }
    Some(byte)
}
