#![allow(clippy::cast_possible_truncation)]

use std::sync::{Arc, Mutex};

use dashmap::DashMap;
use ropey::{Rope, RopeSlice};
use serde_json::Value;

#[derive(Clone, Default)]
pub struct Documents {
    inner: Arc<DashMap<String, Document>>,
}

struct Document {
    rope: Rope,
    cached_text: Mutex<Option<Arc<String>>>,
}

impl Documents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn handle_message(&self, parsed: &Value) {
        let Some(method) = parsed.get("method").and_then(Value::as_str) else {
            return;
        };
        let Some(params) = parsed.get("params") else {
            return;
        };
        match method {
            "textDocument/didOpen" => self.do_open(params),
            "textDocument/didChange" => self.do_change(params),
            "textDocument/didClose" => self.do_close(params),
            _ => {}
        }
    }

    pub fn line_length_utf16(&self, uri: &str, line: u64) -> Option<u64> {
        let doc = self.inner.get(uri)?;
        line_len_utf16(&doc.rope, line as usize)
    }

    pub fn line_text(&self, uri: &str, line: u64) -> Option<String> {
        let doc = self.inner.get(uri)?;
        let line_idx = line as usize;
        if line_idx >= doc.rope.len_lines() {
            return None;
        }
        Some(trim_line_break(doc.rope.line(line_idx)).to_string())
    }

    pub fn full_text(&self, uri: &str) -> Option<Arc<String>> {
        let rope_clone = {
            let doc = self.inner.get(uri)?;
            if let Some(cached) = doc.cached_text.lock().unwrap().as_ref() {
                return Some(cached.clone());
            }
            doc.rope.clone()
        };
        let materialized = Arc::new(rope_clone.to_string());
        if let Some(doc) = self.inner.get(uri) {
            *doc.cached_text.lock().unwrap() = Some(materialized.clone());
        }
        Some(materialized)
    }

    pub fn line_count(&self, uri: &str) -> Option<u64> {
        self.inner
            .get(uri)
            .map(|d| (d.rope.len_lines() as u64).max(1))
    }

    fn do_open(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let text = params
            .pointer("/textDocument/text")
            .and_then(Value::as_str)
            .unwrap_or("");
        let rope = Rope::from_str(text);
        self.inner.insert(
            uri.to_string(),
            Document {
                rope,
                cached_text: Mutex::new(None),
            },
        );
    }

    fn do_change(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        let Some(changes) = params.get("contentChanges").and_then(Value::as_array) else {
            return;
        };
        let Some(mut doc) = self.inner.get_mut(uri) else {
            return;
        };
        for change in changes {
            apply_change(&mut doc.rope, change);
        }
        *doc.cached_text.lock().unwrap() = None;
    }

    fn do_close(&self, params: &Value) {
        let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str) else {
            return;
        };
        self.inner.remove(uri);
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
