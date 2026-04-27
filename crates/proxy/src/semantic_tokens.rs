#![allow(clippy::cast_possible_truncation)]

use std::collections::HashSet;

use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::{
    documents::Documents,
    lsp::{LspHandle, SemanticTokensRequest},
    state::StateHandle,
};

const DECL_KEYWORDS: &[&str] = &[
    "def", "theorem", "lemma", "example", "instance", "abbrev", "axiom",
];

#[derive(Clone, Copy, Debug)]
struct Token {
    line: u32,
    start: u32,
    length: u32,
    ty: u32,
    modifiers: u32,
}

pub fn spawn(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    requests: mpsc::Receiver<SemanticTokensRequest>,
) {
    tokio::spawn(run(lsp, state, documents, requests));
}

async fn run(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    mut requests: mpsc::Receiver<SemanticTokensRequest>,
) {
    while let Some(req) = requests.recv().await {
        let lsp = lsp.clone();
        let state = state.clone();
        let documents = documents.clone();
        tokio::spawn(async move {
            handle_request(lsp, state, documents, req).await;
        });
    }
}

async fn handle_request(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    req: SemanticTokensRequest,
) {
    let id = lsp.alloc_id();
    let lake = lsp
        .request_with_id(&id, "textDocument/semanticTokens/full", req.params.clone())
        .await
        .unwrap_or(Value::Null);

    let uri = req
        .params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str);
    let text = match uri {
        Some(u) => documents.full_text(u).await,
        None => None,
    };
    let Some(text) = text else {
        let _ = lsp.respond_to_client(req.id, lake).await;
        return;
    };

    let mut tokens = decode_tokens(&lake);
    if let Some(comment_idx) = state.token_type_index("comment").await {
        let doc_bit = state
            .token_modifier_index("documentation")
            .await
            .map(|i| 1u32 << i);
        let ranges = scan_comment_ranges(&text);
        let lines_with_lake_token: HashSet<u32> = tokens.iter().map(|t| t.line).collect();

        if let Some(doc_bit) = doc_bit {
            for token in &mut tokens {
                if ranges
                    .iter()
                    .any(|r| r.kind == CommentKind::Doc && r.covers(token.line, token.start))
                {
                    token.modifiers |= doc_bit;
                }
            }
        }
        for range in &ranges {
            let modifiers = match range.kind {
                CommentKind::Doc => doc_bit.unwrap_or(0),
                CommentKind::Plain => 0,
            };
            for line in range.start_line..=range.end_line {
                if lines_with_lake_token.contains(&line) {
                    continue;
                }
                let line_text = text
                    .split('\n')
                    .nth(line as usize)
                    .map_or("", |l| l.trim_end_matches('\r'));
                let line_len = line_text.encode_utf16().count() as u32;
                let start = if line == range.start_line {
                    range.start_col
                } else {
                    0
                };
                let end = if line == range.end_line {
                    range.end_col
                } else {
                    line_len
                };
                if end > start {
                    tokens.push(Token {
                        line,
                        start,
                        length: end - start,
                        ty: comment_idx,
                        modifiers,
                    });
                }
            }
        }
    }

    let existing: HashSet<(u32, u32)> = tokens.iter().map(|t| (t.line, t.start)).collect();

    let kinds = ScanKinds {
        number: state.token_type_index("number").await,
        decl_name: state
            .token_type_index("function")
            .await
            .or(state.token_type_index("method").await),
        capitalized: state.token_type_index("type").await,
        member: state
            .token_type_index("property")
            .await
            .or(state.token_type_index("method").await),
    };

    let mut additions: Vec<Token> = Vec::new();
    scan_lines_unified(&text, &kinds, &mut additions);

    additions.retain(|t| !existing.contains(&(t.line, t.start)));
    tokens.extend(additions);

    tokens.sort_by_key(|t| (t.line, t.start));
    let data = encode_tokens(&tokens);

    let mut response = lake.as_object().cloned().unwrap_or_default();
    response.insert("data".to_string(), Value::Array(data));
    let _ = lsp.respond_to_client(req.id, Value::Object(response)).await;
}

fn decode_tokens(lake: &Value) -> Vec<Token> {
    let Some(data) = lake.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    let nums: Vec<u32> = data
        .iter()
        .map(|v| u32::try_from(v.as_u64().unwrap_or(0)).unwrap_or(0))
        .collect();
    let mut out = Vec::with_capacity(nums.len() / 5);
    let mut line = 0u32;
    let mut start = 0u32;
    for chunk in nums.chunks_exact(5) {
        let dl = chunk[0];
        let ds = chunk[1];
        if dl > 0 {
            line += dl;
            start = ds;
        } else {
            start += ds;
        }
        out.push(Token {
            line,
            start,
            length: chunk[2],
            ty: chunk[3],
            modifiers: chunk[4],
        });
    }
    out
}

fn encode_tokens(tokens: &[Token]) -> Vec<Value> {
    let mut out = Vec::with_capacity(tokens.len() * 5);
    let mut prev_line = 0u32;
    let mut prev_start = 0u32;
    for t in tokens {
        let dl = t.line - prev_line;
        let ds = if dl > 0 {
            t.start
        } else {
            t.start - prev_start
        };
        out.push(json!(dl));
        out.push(json!(ds));
        out.push(json!(t.length));
        out.push(json!(t.ty));
        out.push(json!(t.modifiers));
        prev_line = t.line;
        prev_start = t.start;
    }
    out
}

struct ScanKinds {
    number: Option<u32>,
    decl_name: Option<u32>,
    capitalized: Option<u32>,
    member: Option<u32>,
}

impl ScanKinds {
    fn any(&self) -> bool {
        self.number.is_some()
            || self.decl_name.is_some()
            || self.capitalized.is_some()
            || self.member.is_some()
    }
}

fn scan_lines_unified(text: &str, kinds: &ScanKinds, out: &mut Vec<Token>) {
    if !kinds.any() {
        return;
    }
    for (line_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end_matches('\r');
        if let Some(ty) = kinds.decl_name {
            scan_decl_in_line(line, line_idx, ty, out);
        }
        if let Some(ty) = kinds.number {
            scan_numbers_in_line(line, line_idx, ty, out);
        }
        if let Some(ty) = kinds.capitalized {
            scan_caps_in_line(line, line_idx, ty, out);
        }
        if let Some(ty) = kinds.member {
            scan_member_in_line(line, line_idx, ty, out);
        }
    }
}

fn scan_numbers_in_line(line: &str, line_idx: usize, ty: u32, out: &mut Vec<Token>) {
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let prev_ident = i > 0 && line[..i].chars().next_back().is_some_and(is_ident_continue);
            if !prev_ident {
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let next_ident =
                    i < bytes.len() && line[i..].chars().next().is_some_and(is_ident_continue);
                if !next_ident {
                    out.push(make_token(line, line_idx, start, i, ty));
                }
                continue;
            }
        }
        i += line[i..].chars().next().map_or(1, char::len_utf8);
    }
}

fn scan_caps_in_line(line: &str, line_idx: usize, ty: u32, out: &mut Vec<Token>) {
    let mut i = 0;
    while i < line.len() {
        let Some(c) = line[i..].chars().next() else {
            break;
        };
        if c.is_uppercase() {
            let prev_ident = i > 0 && line[..i].chars().next_back().is_some_and(is_ident_continue);
            if !prev_ident {
                let start = i;
                let mut end = i;
                for (off, ch) in line[i..].char_indices() {
                    if !is_ident_continue(ch) {
                        break;
                    }
                    end = i + off + ch.len_utf8();
                }
                out.push(make_token(line, line_idx, start, end, ty));
                i = end.max(i + c.len_utf8());
                continue;
            }
        }
        i += c.len_utf8();
    }
}

fn scan_member_in_line(line: &str, line_idx: usize, ty: u32, out: &mut Vec<Token>) {
    for (i, c) in line.char_indices() {
        if c != '.' {
            continue;
        }
        if line[i + 1..].starts_with('.') {
            continue;
        }
        let after = i + 1;
        let Some(first) = line[after..].chars().next() else {
            continue;
        };
        if !is_ident_start(first) {
            continue;
        }
        let mut end = after;
        for (off, ch) in line[after..].char_indices() {
            if !is_ident_continue(ch) {
                break;
            }
            end = after + off + ch.len_utf8();
        }
        if end > after {
            out.push(make_token(line, line_idx, after, end, ty));
        }
    }
}

fn scan_decl_in_line(line: &str, line_idx: usize, ty: u32, out: &mut Vec<Token>) {
    let trimmed = line.trim_start();
    let leading = line.len() - trimmed.len();
    for kw in DECL_KEYWORDS {
        let Some(rest) = trimmed.strip_prefix(kw) else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_whitespace()) {
            continue;
        }
        let after_kw = leading + kw.len();
        let name_offset = line[after_kw..]
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(0);
        let name_start = after_kw + name_offset;
        let name_end = line[name_start..]
            .char_indices()
            .find(|(_, c)| !is_ident_continue(*c))
            .map_or(line.len(), |(i, _)| name_start + i);
        if name_end > name_start {
            out.push(make_token(line, line_idx, name_start, name_end, ty));
        }
        return;
    }
}

fn make_token(line_str: &str, line: usize, start: usize, end: usize, ty: u32) -> Token {
    let utf16_start = line_str[..start].encode_utf16().count() as u32;
    let utf16_len = line_str[start..end].encode_utf16().count() as u32;
    Token {
        line: line as u32,
        start: utf16_start,
        length: utf16_len,
        ty,
        modifiers: 0,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum CommentKind {
    Doc,
    Plain,
}

struct CommentRange {
    kind: CommentKind,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

impl CommentRange {
    fn covers(&self, line: u32, col: u32) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        if line == self.start_line && col < self.start_col {
            return false;
        }
        if line == self.end_line && col >= self.end_col {
            return false;
        }
        true
    }
}

fn scan_comment_ranges(text: &str) -> Vec<CommentRange> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' {
            i += 1;
            while i < bytes.len() {
                match bytes[i] {
                    b'\\' if i + 1 < bytes.len() => i += 2,
                    b'"' => {
                        i += 1;
                        break;
                    }
                    _ => i += 1,
                }
            }
            continue;
        }

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let start_byte = i;
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            let (sl, sc) = byte_to_line_col_utf16(text, start_byte);
            let (el, ec) = byte_to_line_col_utf16(text, j);
            out.push(CommentRange {
                kind: CommentKind::Plain,
                start_line: sl,
                start_col: sc,
                end_line: el,
                end_col: ec,
            });
            i = j;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            let kind = if i + 2 < bytes.len() && (bytes[i + 2] == b'-' || bytes[i + 2] == b'!') {
                CommentKind::Doc
            } else {
                CommentKind::Plain
            };
            let start_byte = i;
            let mut j = i + 2;
            let mut depth: u32 = 1;
            while j + 1 < bytes.len() && depth > 0 {
                if bytes[j] == b'/' && bytes[j + 1] == b'-' {
                    depth += 1;
                    j += 2;
                } else if bytes[j] == b'-' && bytes[j + 1] == b'/' {
                    depth -= 1;
                    j += 2;
                } else {
                    j += 1;
                }
            }
            let end_byte = j;
            let (sl, sc) = byte_to_line_col_utf16(text, start_byte);
            let (el, ec) = byte_to_line_col_utf16(text, end_byte);
            out.push(CommentRange {
                kind,
                start_line: sl,
                start_col: sc,
                end_line: el,
                end_col: ec,
            });
            i = end_byte;
            continue;
        }

        i += 1;
    }
    out
}

fn byte_to_line_col_utf16(text: &str, byte_offset: usize) -> (u32, u32) {
    let clamped = byte_offset.min(text.len());
    let mut line = 0u32;
    let mut last_newline_byte = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if i >= clamped {
            break;
        }
        if b == b'\n' {
            line += 1;
            last_newline_byte = i + 1;
        }
    }
    let col = text[last_newline_byte..clamped].encode_utf16().count() as u32;
    (line, col)
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '\''
}
