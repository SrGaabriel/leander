#![allow(clippy::cast_possible_truncation)]

use std::{collections::HashMap, sync::Arc};

use ropey::{Rope, RopeSlice};
use serde_json::Value;
use tokio::sync::RwLock;

#[derive(Clone, Default)]
pub struct Documents {
    inner: Arc<RwLock<HashMap<String, Document>>>,
}

struct Document {
    rope: Rope,
}

impl Documents {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn handle_message(&self, parsed: &Value) {
        let Some(method) = parsed.get("method").and_then(Value::as_str) else {
            return;
        };
        let Some(params) = parsed.get("params") else {
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
        line_len_utf16(&doc.rope, line as usize)
    }

    pub async fn line_text(&self, uri: &str, line: u64) -> Option<String> {
        let docs = self.inner.read().await;
        let doc = docs.get(uri)?;
        let line_idx = line as usize;
        if line_idx >= doc.rope.len_lines() {
            return None;
        }
        let slice = trim_line_break(doc.rope.line(line_idx));
        Some(slice.to_string())
    }

    pub async fn full_text(&self, uri: &str) -> Option<String> {
        let docs = self.inner.read().await;
        docs.get(uri).map(|d| d.rope.to_string())
    }

    pub async fn line_count(&self, uri: &str) -> Option<u64> {
        let docs = self.inner.read().await;
        docs.get(uri).map(|d| (d.rope.len_lines() as u64).max(1))
    }

    async fn do_open(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let text = params
            .pointer("/textDocument/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let rope = Rope::from_str(text);
        self.inner
            .write()
            .await
            .insert(uri.to_string(), Document { rope });
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
            apply_change(&mut doc.rope, change);
        }
    }

    async fn do_close(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        self.inner.write().await.remove(uri);
    }
}

fn trim_line_break(slice: RopeSlice<'_>) -> RopeSlice<'_> {
    let mut end = slice.len_chars();
    if end == 0 {
        return slice;
    }
    if slice.char(end - 1) == '\n' {
        end -= 1;
        if end > 0 && slice.char(end - 1) == '\r' {
            end -= 1;
        }
    }
    slice.slice(..end)
}

fn line_len_utf16(rope: &Rope, line: usize) -> Option<u64> {
    if line >= rope.len_lines() {
        return None;
    }
    Some(trim_line_break(rope.line(line)).len_utf16_cu() as u64)
}

fn apply_change(rope: &mut Rope, change: &Value) {
    let new_text = change.get("text").and_then(Value::as_str).unwrap_or("");
    match change.get("range") {
        None => {
            *rope = Rope::from_str(new_text);
        }
        Some(range) => {
            let start = position_to_char(rope, range.get("start"));
            let end = position_to_char(rope, range.get("end"));
            if let (Some(start), Some(end)) = (start, end)
                && start <= end
                && end <= rope.len_chars()
            {
                rope.remove(start..end);
                rope.insert(start, new_text);
            }
        }
    }
}

fn position_to_char(rope: &Rope, pos: Option<&Value>) -> Option<usize> {
    let pos = pos?;
    let line = pos.get("line")?.as_u64()? as usize;
    let character = pos.get("character")?.as_u64()? as usize;

    if line >= rope.len_lines() {
        return Some(rope.len_chars());
    }

    let line_start_char = rope.line_to_char(line);
    let line_start_cu = rope.char_to_utf16_cu(line_start_char);
    let target_cu = line_start_cu + character;

    let line_end_char = if line + 1 < rope.len_lines() {
        rope.line_to_char(line + 1)
    } else {
        rope.len_chars()
    };
    let line_end_cu = rope.char_to_utf16_cu(line_end_char);

    let cu = target_cu.min(line_end_cu);
    Some(rope.utf16_cu_to_char(cu))
}
