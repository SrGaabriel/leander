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
    let existing: HashSet<(u32, u32)> = tokens.iter().map(|t| (t.line, t.start)).collect();

    let mut additions: Vec<Token> = Vec::new();
    if let Some(idx) = state.token_type_index("number").await {
        scan_numbers(&text, idx, &mut additions);
    }
    if let Some(idx) = state
        .token_type_index("function")
        .await
        .or(state.token_type_index("method").await)
    {
        scan_decl_names(&text, idx, &mut additions);
    }
    if let Some(idx) = state.token_type_index("type").await {
        scan_capitalized_idents(&text, idx, &mut additions);
    }
    if let Some(idx) = state
        .token_type_index("property")
        .await
        .or(state.token_type_index("method").await)
    {
        scan_member_access(&text, idx, &mut additions);
    }

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

fn scan_numbers(text: &str, ty: u32, out: &mut Vec<Token>) {
    for (line_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end_matches('\r');
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i].is_ascii_digit() {
                let prev_ident =
                    i > 0 && line[..i].chars().next_back().is_some_and(is_ident_continue);
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
}

fn scan_decl_names(text: &str, ty: u32, out: &mut Vec<Token>) {
    for (line_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end_matches('\r');
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
            break;
        }
    }
}

fn scan_capitalized_idents(text: &str, ty: u32, out: &mut Vec<Token>) {
    for (line_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end_matches('\r');
        let mut i = 0;
        while i < line.len() {
            let Some((_, c)) = line[i..].char_indices().next() else {
                break;
            };
            if c.is_uppercase() {
                let prev_ident =
                    i > 0 && line[..i].chars().next_back().is_some_and(is_ident_continue);
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
                    i = end;
                    continue;
                }
            }
            i += c.len_utf8();
        }
    }
}

fn scan_member_access(text: &str, ty: u32, out: &mut Vec<Token>) {
    for (line_idx, raw_line) in text.split('\n').enumerate() {
        let line = raw_line.trim_end_matches('\r');
        for (i, c) in line.char_indices() {
            if c != '.' {
                continue;
            }
            if line[i + 1..].starts_with('.') {
                continue;
            }
            let after = i + 1;
            let Some((_, first)) = line[after..].char_indices().next() else {
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

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '\''
}
