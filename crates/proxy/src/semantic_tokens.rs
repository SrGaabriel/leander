#![allow(clippy::cast_possible_truncation)]

use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use serde_json::{Value, json};
use tokio::sync::{Mutex, mpsc};

use crate::{
    code_lens::DECL_KEYWORDS,
    documents::Documents,
    lsp::{LspHandle, SemanticTokensRequest},
    state::{StateEvent, StateHandle},
};

#[derive(Clone, Copy, Debug)]
struct Token {
    line: u32,
    start: u32,
    length: u32,
    ty: u32,
    modifiers: u32,
}

#[derive(Default)]
struct ScanCache {
    entries: HashMap<String, CachedScan>,
}

struct CachedScan {
    version: i64,
    lex: Arc<LexRanges>,
    additions: Arc<Vec<Token>>,
}

pub fn spawn(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    requests: mpsc::Receiver<SemanticTokensRequest>,
) {
    let cache = Arc::new(Mutex::new(ScanCache::default()));
    tokio::spawn(invalidate_on_close(cache.clone(), state.clone()));
    tokio::spawn(run(lsp, state, documents, requests, cache));
}

async fn invalidate_on_close(cache: Arc<Mutex<ScanCache>>, state: StateHandle) {
    let mut events = state.subscribe();
    while let Ok(ev) = events.recv().await {
        if let StateEvent::DidClose { uri } = ev {
            cache.lock().await.entries.remove(&uri);
        }
    }
}

async fn run(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    mut requests: mpsc::Receiver<SemanticTokensRequest>,
    cache: Arc<Mutex<ScanCache>>,
) {
    while let Some(req) = requests.recv().await {
        let lsp = lsp.clone();
        let state = state.clone();
        let documents = documents.clone();
        let cache = cache.clone();
        tokio::spawn(async move {
            handle_request(lsp, state, documents, req, cache).await;
        });
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_request(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    req: SemanticTokensRequest,
    cache: Arc<Mutex<ScanCache>>,
) {
    let id = lsp.alloc_id();
    let lake = lsp
        .request_with_id(&id, "textDocument/semanticTokens/full", req.params.clone())
        .await
        .unwrap_or(Value::Null);

    let uri = req
        .params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .map(str::to_string);
    let Some(uri) = uri else {
        let _ = lsp.respond_to_client(req.id, lake).await;
        return;
    };

    let comment_idx = state.token_type_index("comment");
    let string_idx = state.token_type_index("string");
    let kinds = ScanKinds {
        number: state.token_type_index("number"),
        decl_name: state
            .token_type_index("function")
            .or(state.token_type_index("method")),
        capitalized: state.token_type_index("type"),
        member: state
            .token_type_index("property")
            .or(state.token_type_index("method")),
    };

    let current_version = state.version_for(&uri).unwrap_or(0);

    let cached = {
        let cache_guard = cache.lock().await;
        cache_guard.entries.get(&uri).and_then(|s| {
            if s.version == current_version {
                Some((s.lex.clone(), s.additions.clone()))
            } else {
                None
            }
        })
    };

    let Some(text) = documents.full_text(&uri) else {
        let _ = lsp.respond_to_client(req.id, lake).await;
        return;
    };

    let (lex, additions_all) = if let Some(c) = cached {
        c
    } else {
        let lex = if comment_idx.is_some() || string_idx.is_some() {
            Arc::new(scan_lex_ranges(&text))
        } else {
            Arc::new(LexRanges::default())
        };
        let mut additions: Vec<Token> = Vec::new();
        scan_lines_unified(&text, &kinds, &mut additions);
        let additions = Arc::new(additions);
        cache.lock().await.entries.insert(
            uri.clone(),
            CachedScan {
                version: current_version,
                lex: lex.clone(),
                additions: additions.clone(),
            },
        );
        (lex, additions)
    };

    let lines: Vec<&str> = text.split('\n').map(|l| l.trim_end_matches('\r')).collect();

    let mut tokens = decode_tokens(&lake);
    let lake_token_positions: HashSet<(u32, u32)> =
        tokens.iter().map(|t| (t.line, t.start)).collect();

    if let Some(comment_idx) = comment_idx {
        let doc_bit = state
            .token_modifier_index("documentation")
            .map(|i| 1u32 << i);

        if let Some(doc_bit) = doc_bit {
            let doc_index = RangeLineIndex::from_iter(
                lex.comments
                    .iter()
                    .filter(|r| r.kind == CommentKind::Doc)
                    .map(CommentRange::span),
            );
            for token in &mut tokens {
                if doc_index.covers(token.line, token.start) {
                    token.modifiers |= doc_bit;
                }
            }
        }
        for range in &lex.comments {
            let modifiers = match range.kind {
                CommentKind::Doc => doc_bit.unwrap_or(0),
                CommentKind::Plain => 0,
            };
            emit_range_tokens(
                &lines,
                &lake_token_positions,
                range.start_line,
                range.start_col,
                range.end_line,
                range.end_col,
                comment_idx,
                modifiers,
                &mut tokens,
            );
        }
    }

    if let Some(string_idx) = string_idx {
        for range in &lex.strings {
            emit_range_tokens(
                &lines,
                &lake_token_positions,
                range.start_line,
                range.start_col,
                range.end_line,
                range.end_col,
                string_idx,
                0,
                &mut tokens,
            );
        }
    }

    let existing: HashSet<(u32, u32)> = tokens.iter().map(|t| (t.line, t.start)).collect();
    let strings_index = RangeLineIndex::from_iter(lex.strings.iter().map(StringRange::span));
    let comments_index = RangeLineIndex::from_iter(lex.comments.iter().map(CommentRange::span));

    for t in additions_all.iter() {
        if !existing.contains(&(t.line, t.start))
            && !strings_index.covers(t.line, t.start)
            && !comments_index.covers(t.line, t.start)
        {
            tokens.push(*t);
        }
    }

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

#[derive(Clone, Copy)]
struct Span {
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

struct CommentRange {
    kind: CommentKind,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

impl CommentRange {
    fn span(&self) -> Span {
        Span {
            start_line: self.start_line,
            start_col: self.start_col,
            end_line: self.end_line,
            end_col: self.end_col,
        }
    }
}

struct StringRange {
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
}

impl StringRange {
    fn span(&self) -> Span {
        Span {
            start_line: self.start_line,
            start_col: self.start_col,
            end_line: self.end_line,
            end_col: self.end_col,
        }
    }
}

#[derive(Default)]
struct LexRanges {
    comments: Vec<CommentRange>,
    strings: Vec<StringRange>,
}

struct RangeLineIndex {
    by_line: HashMap<u32, Vec<(u32, u32)>>,
}

impl RangeLineIndex {
    fn from_iter<I: IntoIterator<Item = Span>>(iter: I) -> Self {
        let mut by_line: HashMap<u32, Vec<(u32, u32)>> = HashMap::new();
        for span in iter {
            for line in span.start_line..=span.end_line {
                let start = if line == span.start_line {
                    span.start_col
                } else {
                    0
                };
                let end = if line == span.end_line {
                    span.end_col
                } else {
                    u32::MAX
                };
                if end > start {
                    by_line.entry(line).or_default().push((start, end));
                }
            }
        }
        Self { by_line }
    }

    fn covers(&self, line: u32, col: u32) -> bool {
        let Some(spans) = self.by_line.get(&line) else {
            return false;
        };
        spans.iter().any(|(s, e)| col >= *s && col < *e)
    }
}

struct LexScanner<'a> {
    text: &'a str,
    bytes: &'a [u8],
    i: usize,
    line: u32,
    line_start_byte: usize,
}

impl<'a> LexScanner<'a> {
    fn new(text: &'a str) -> Self {
        Self {
            text,
            bytes: text.as_bytes(),
            i: 0,
            line: 0,
            line_start_byte: 0,
        }
    }

    fn pos(&self, byte_offset: usize) -> (u32, u32) {
        let clamped = byte_offset.min(self.bytes.len());
        if clamped >= self.line_start_byte {
            let col = self.text[self.line_start_byte..clamped]
                .encode_utf16()
                .count() as u32;
            (self.line, col)
        } else {
            byte_to_line_col_utf16(self.text, clamped)
        }
    }

    fn advance_to(&mut self, target: usize) {
        let target = target.min(self.bytes.len());
        while self.i < target {
            if self.bytes[self.i] == b'\n' {
                self.line += 1;
                self.line_start_byte = self.i + 1;
            }
            self.i += 1;
        }
    }
}

fn scan_lex_ranges(text: &str) -> LexRanges {
    let bytes = text.as_bytes();
    let mut out = LexRanges::default();
    let mut s = LexScanner::new(text);

    while s.i < bytes.len() {
        let i = s.i;
        let b = bytes[i];

        if (b == b's' || b == b'm')
            && bytes.get(i + 1) == Some(&b'!')
            && bytes.get(i + 2) == Some(&b'"')
            && !prev_byte_is_ident_continue(bytes, i)
        {
            let body_end = consume_quoted(bytes, i + 3);
            push_string(&mut s, &mut out.strings, i, body_end);
            s.advance_to(body_end);
            continue;
        }

        if b == b'r' && !prev_byte_is_ident_continue(bytes, i) {
            let mut hashes = 0usize;
            let mut k = i + 1;
            while bytes.get(k) == Some(&b'#') {
                hashes += 1;
                k += 1;
            }
            if bytes.get(k) == Some(&b'"') {
                let body_end = consume_raw(bytes, k + 1, hashes);
                push_string(&mut s, &mut out.strings, i, body_end);
                s.advance_to(body_end);
                continue;
            }
        }

        if b == b'"' {
            let body_end = consume_quoted(bytes, i + 1);
            push_string(&mut s, &mut out.strings, i, body_end);
            s.advance_to(body_end);
            continue;
        }

        if b == b'\''
            && !prev_byte_is_ident_continue(bytes, i)
            && let Some(end) = consume_char_lit(bytes, i + 1)
        {
            push_string(&mut s, &mut out.strings, i, end);
            s.advance_to(end);
            continue;
        }

        if b == b'-' && bytes.get(i + 1) == Some(&b'-') {
            let mut j = i + 2;
            while j < bytes.len() && bytes[j] != b'\n' {
                j += 1;
            }
            let (sl, sc) = s.pos(i);
            s.advance_to(j);
            let (el, ec) = s.pos(j);
            out.comments.push(CommentRange {
                kind: CommentKind::Plain,
                start_line: sl,
                start_col: sc,
                end_line: el,
                end_col: ec,
            });
            continue;
        }

        if b == b'/' && bytes.get(i + 1) == Some(&b'-') {
            let kind = if matches!(bytes.get(i + 2), Some(&b'-' | &b'!')) {
                CommentKind::Doc
            } else {
                CommentKind::Plain
            };
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
            let (sl, sc) = s.pos(i);
            s.advance_to(j);
            let (el, ec) = s.pos(j);
            out.comments.push(CommentRange {
                kind,
                start_line: sl,
                start_col: sc,
                end_line: el,
                end_col: ec,
            });
            continue;
        }

        s.advance_to(i + 1);
    }
    out
}

fn prev_byte_is_ident_continue(bytes: &[u8], i: usize) -> bool {
    if i == 0 {
        return false;
    }
    let prev = bytes[i - 1];
    prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'\''
}

fn consume_quoted(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'"' => return i + 1,
            _ => i += 1,
        }
    }
    bytes.len()
}

fn consume_raw(bytes: &[u8], mut i: usize, hashes: usize) -> usize {
    while i < bytes.len() {
        if bytes[i] == b'"' {
            let mut k = i + 1;
            let mut count = 0;
            while count < hashes && bytes.get(k) == Some(&b'#') {
                count += 1;
                k += 1;
            }
            if count == hashes {
                return k;
            }
        }
        i += 1;
    }
    bytes.len()
}

fn consume_char_lit(bytes: &[u8], mut i: usize) -> Option<usize> {
    let limit = (i + 16).min(bytes.len());
    while i < limit {
        match bytes[i] {
            b'\\' if i + 1 < bytes.len() => i += 2,
            b'\'' => return Some(i + 1),
            b'\n' => return None,
            _ => i += 1,
        }
    }
    None
}

fn push_string(
    s: &mut LexScanner<'_>,
    out: &mut Vec<StringRange>,
    start_byte: usize,
    end_byte: usize,
) {
    if end_byte <= start_byte {
        return;
    }
    let (sl, sc) = s.pos(start_byte);
    s.advance_to(end_byte);
    let (el, ec) = s.pos(end_byte);
    out.push(StringRange {
        start_line: sl,
        start_col: sc,
        end_line: el,
        end_col: ec,
    });
}

#[allow(clippy::too_many_arguments)]
fn emit_range_tokens(
    lines: &[&str],
    lake_positions: &HashSet<(u32, u32)>,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    ty: u32,
    modifiers: u32,
    out: &mut Vec<Token>,
) {
    for line in start_line..=end_line {
        let line_text = lines.get(line as usize).copied().unwrap_or("");
        let line_len = line_text.encode_utf16().count() as u32;
        let start = if line == start_line { start_col } else { 0 };
        let end_raw = if line == end_line { end_col } else { line_len };
        let end = end_raw.min(line_len);
        if end > start && !lake_positions.contains(&(line, start)) {
            out.push(Token {
                line,
                start,
                length: end - start,
                ty,
                modifiers,
            });
        }
    }
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
