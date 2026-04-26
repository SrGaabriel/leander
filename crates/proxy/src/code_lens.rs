#![allow(clippy::cast_possible_truncation)]

use std::{collections::HashMap, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::{
    documents::Documents,
    lsp::{CodeLensRequest, LspHandle},
    state::{SEVERITY_ERROR, SEVERITY_WARNING, StateEvent, StateHandle},
};

const REFRESH_DEBOUNCE: Duration = Duration::from_millis(300);

const DECL_KEYWORDS: &[&str] = &["theorem", "lemma", "example", "def", "instance", "abbrev"];

pub fn spawn(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    requests: mpsc::Receiver<CodeLensRequest>,
) {
    let lens = Arc::new(CodeLens {
        lsp,
        state: state.clone(),
        documents,
        last_response: Mutex::new(HashMap::new()),
    });
    spawn_request_worker(lens.clone(), requests);
    spawn_refresh_worker(lens, state);
}

struct CodeLens {
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    last_response: Mutex<HashMap<String, usize>>,
}

fn spawn_request_worker(lens: Arc<CodeLens>, mut requests: mpsc::Receiver<CodeLensRequest>) {
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            let lens = lens.clone();
            tokio::spawn(async move { lens.handle_request(req).await });
        }
    });
}

fn spawn_refresh_worker(lens: Arc<CodeLens>, state: StateHandle) {
    tokio::spawn(async move {
        let mut events = state.subscribe();
        let mut interval = tokio::time::interval(REFRESH_DEBOUNCE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pending = false;
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(StateEvent::ProgressChanged { .. } | StateEvent::DiagnosticsChanged { .. }) => pending = true,
                    Ok(StateEvent::DidOpen {..} | StateEvent::DidClose { .. })
                    | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = interval.tick() => {
                    if pending {
                        pending = false;
                        let _ = lens
                            .lsp
                            .request_client("workspace/codeLens/refresh", json!(null))
                            .await;
                    }
                }
            }
        }
    });
}

impl CodeLens {
    async fn handle_request(&self, req: CodeLensRequest) {
        let Some(uri) = req
            .params
            .pointer("/textDocument/uri")
            .and_then(Value::as_str)
            .map(str::to_string)
        else {
            let _ = self.lsp.respond_to_client(req.id, json!([])).await;
            return;
        };
        let lenses = self.compute(&uri).await;
        self.last_response
            .lock()
            .await
            .insert(uri.clone(), lenses.len());
        let _ = self
            .lsp
            .respond_to_client(req.id, Value::Array(lenses))
            .await;
    }

    async fn compute(&self, uri: &str) -> Vec<Value> {
        let Some(text) = self.documents.full_text(uri).await else {
            return Vec::new();
        };
        let diagnostics = self.state.diagnostics_for(uri).await;

        let mut lenses = Vec::new();
        for decl in find_declarations(&text) {
            let mut title_parts = Vec::<String>::new();
            let status = decl_status(&diagnostics, decl.start_line, decl.end_line);
            title_parts.push(status.glyph().to_string());

            title_parts.push(format!("{} {}", decl.keyword, decl.name));
            if decl.has_tactic_body {
                let tactic_count = count_tactics(&text, decl.start_line, decl.end_line);
                if tactic_count > 0 {
                    title_parts.push(format!(
                        "{} tactic{}",
                        tactic_count,
                        if tactic_count == 1 { "" } else { "s" }
                    ));
                }
            }

            let title = title_parts.join(" · ");
            lenses.push(json!({
                "range": {
                    "start": { "line": decl.start_line, "character": 0 },
                    "end":   { "line": decl.start_line, "character": 0 },
                },
                "command": {
                    "title": title,
                    "command": "editor.action.peekLocations",
                    "arguments": [],
                },
            }));
        }
        lenses
    }
}

#[derive(Debug)]
struct Declaration {
    keyword: String,
    name: String,
    start_line: u64,
    end_line: u64,
    has_tactic_body: bool,
}

fn find_declarations(text: &str) -> Vec<Declaration> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut starts: Vec<(usize, String, String)> = Vec::new();

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.len() == line.len()
            && let Some((kw, rest)) = parse_decl_keyword(line)
        {
            let name = rest
                .split(|c: char| c.is_whitespace() || c == ':' || c == '(')
                .find(|s| !s.is_empty())
                .unwrap_or("?")
                .to_string();
            starts.push((i, kw.to_string(), name));
        }
    }

    let mut out = Vec::with_capacity(starts.len());
    for (idx, (line_idx, keyword, name)) in starts.iter().enumerate() {
        let end_line = starts.get(idx + 1).map_or_else(
            || lines.len().saturating_sub(1),
            |(next, _, _)| (*next).saturating_sub(1),
        );
        let has_tactic_body = (*line_idx..=end_line).any(|i| {
            lines
                .get(i)
                .is_some_and(|l| l.contains(":= by") || l.trim() == "by")
        });
        out.push(Declaration {
            keyword: keyword.clone(),
            name: name.clone(),
            start_line: *line_idx as u64,
            end_line: end_line as u64,
            has_tactic_body,
        });
    }
    out
}

fn parse_decl_keyword(line: &str) -> Option<(&'static str, &str)> {
    for kw in DECL_KEYWORDS {
        if let Some(rest) = line.strip_prefix(kw)
            && rest.starts_with(|c: char| c.is_whitespace())
        {
            return Some((kw, rest.trim_start()));
        }
    }
    None
}

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Error,
    Warning,
}

impl Status {
    fn glyph(self) -> &'static str {
        match self {
            Status::Ok => "✓",
            Status::Error => "✗",
            Status::Warning => "⚠",
        }
    }
}

fn decl_status(diagnostics: &[Value], start: u64, end: u64) -> Status {
    let mut worst = Status::Ok;
    for d in diagnostics {
        let l = d.pointer("/range/start/line").and_then(Value::as_u64);
        let Some(l) = l else { continue };
        if l < start || l > end {
            continue;
        }
        let sev = d.get("severity").and_then(Value::as_i64).unwrap_or(0);
        match sev {
            v if v == SEVERITY_ERROR => return Status::Error,
            v if v == SEVERITY_WARNING => worst = Status::Warning,
            _ => {}
        }
    }
    worst
}

fn count_tactics(text: &str, start_line: u64, end_line: u64) -> usize {
    text.split('\n')
        .skip(start_line as usize + 1)
        .take((end_line - start_line) as usize)
        .filter(|l| {
            let t = l.trim();
            !t.is_empty() && !t.starts_with("--") && !t.starts_with("/-") && !t.starts_with('|')
        })
        .count()
}
