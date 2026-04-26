use std::{collections::HashMap, sync::Arc, time::Duration};

use serde_json::{Value, json};
use tokio::sync::{Mutex, broadcast, mpsc};

use crate::{
    documents::Documents,
    lsp::{InlayRequest, LspHandle},
    state::{SEVERITY_ERROR, SEVERITY_WARNING, StateEvent, StateHandle},
};

const REFRESH_DEBOUNCE: Duration = Duration::from_millis(250);

const INLINE_TARGET_LIMIT: usize = 70;

pub fn spawn(
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    requests: mpsc::Receiver<InlayRequest>,
) {
    let inlay = Arc::new(Inlay {
        lsp,
        state: state.clone(),
        documents,
        cache: Mutex::new(HashMap::new()),
    });
    spawn_request_worker(inlay.clone(), requests);
    spawn_refresh_worker(inlay, state);
}

type InlayCache = HashMap<String, HashMap<(u64, u64), CachedProbe>>;

struct Inlay {
    lsp: LspHandle,
    state: StateHandle,
    documents: Documents,
    cache: Mutex<InlayCache>,
}

#[derive(Clone)]
struct CachedProbe {
    version: i64,
    value: Option<Value>,
}

fn spawn_request_worker(inlay: Arc<Inlay>, mut requests: mpsc::Receiver<InlayRequest>) {
    tokio::spawn(async move {
        while let Some(req) = requests.recv().await {
            let inlay = inlay.clone();
            tokio::spawn(async move { handle_request(inlay, req).await });
        }
    });
}

fn spawn_refresh_worker(inlay: Arc<Inlay>, state: StateHandle) {
    tokio::spawn(async move {
        let mut events = state.subscribe();
        let mut interval = tokio::time::interval(REFRESH_DEBOUNCE);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut pending = false;
        loop {
            tokio::select! {
                ev = events.recv() => match ev {
                    Ok(StateEvent::ProgressChanged { uri }) => {
                        let frontier = inlay.state.elaboration_frontier(&uri).await;
                        if frontier == u64::MAX {
                            pending = false;
                            let _ = inlay
                                .lsp
                                .request_client("workspace/inlayHint/refresh", json!(null))
                                .await;
                        } else {
                            pending = true;
                        }
                    }
                    Ok(StateEvent::DiagnosticsChanged { .. }) => {
                        pending = true;
                    }
                    Ok(StateEvent::DidClose { uri }) => {
                        inlay.invalidate(&uri).await;
                    }
                    Ok(StateEvent::DidOpen { .. })
                    | Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                },
                _ = interval.tick() => {
                    if pending {
                        pending = false;
                        let _ = inlay
                            .lsp
                            .request_client("workspace/inlayHint/refresh", json!(null))
                            .await;
                    }
                }
            }
        }
    });
}

async fn handle_request(inlay: Arc<Inlay>, req: InlayRequest) {
    let Some(uri) = req
        .params
        .pointer("/textDocument/uri")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        let _ = inlay.lsp.respond_to_client(req.id, json!([])).await;
        return;
    };
    let start_line = req
        .params
        .pointer("/range/start/line")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let end_line = req
        .params
        .pointer("/range/end/line")
        .and_then(Value::as_u64)
        .unwrap_or(start_line);

    let hints = compute_hints_concurrently(inlay.clone(), &uri, start_line, end_line).await;
    let _ = inlay
        .lsp
        .respond_to_client(req.id, Value::Array(hints))
        .await;
}

async fn compute_hints_concurrently(
    inlay: Arc<Inlay>,
    uri: &str,
    start_line: u64,
    end_line: u64,
) -> Vec<Value> {
    use tokio::task::JoinSet;

    let frontier = inlay.state.elaboration_frontier(uri).await;
    let mut set: JoinSet<Option<Vec<Value>>> = JoinSet::new();
    for line in start_line..=end_line {
        let inlay = inlay.clone();
        let uri = uri.to_string();
        set.spawn(async move { hints_for_line(inlay, uri, line, frontier).await });
    }

    let mut grouped: Vec<(u64, Vec<Value>)> = Vec::new();
    while let Some(res) = set.join_next().await {
        if let Ok(Some(hints)) = res
            && let Some(line) = hints
                .first()
                .and_then(|h| h.pointer("/position/line"))
                .and_then(Value::as_u64)
        {
            grouped.push((line, hints));
        }
    }
    grouped.sort_by_key(|(line, _)| *line);
    grouped.into_iter().flat_map(|(_, h)| h).collect()
}

async fn hints_for_line(
    inlay: Arc<Inlay>,
    uri: String,
    line: u64,
    frontier: u64,
) -> Option<Vec<Value>> {
    let line_text = inlay.documents.line_text(&uri, line).await?;
    let trimmed = line_text.trim_start();
    let line_len = inlay.documents.line_length_utf16(&uri, line).await?;

    let mut out = Vec::new();
    let probe_eligible = line < frontier
        && !trimmed.is_empty()
        && !trimmed.starts_with("--")
        && !trimmed.starts_with("/-");

    let entering = if probe_eligible {
        inlay.probe(&uri, line, 0).await
    } else {
        None
    };
    let exiting = if probe_eligible && entering.is_some() {
        inlay.probe(&uri, line, line_len).await
    } else {
        None
    };
    let diagnostics = inlay.state.diagnostics_at_line(&uri, line).await;

    if let Some(hint) = build_hint(
        line,
        line_len,
        entering.as_ref(),
        exiting.as_ref(),
        &diagnostics,
    ) {
        out.push(hint);
    }
    if let Some(s) = inlay.sorry_hint(&uri, line).await {
        out.push(s);
    }

    if out.is_empty() { None } else { Some(out) }
}

impl Inlay {
    async fn invalidate(&self, uri: &str) {
        self.cache.lock().await.remove(uri);
    }

    async fn sorry_hint(&self, uri: &str, line: u64) -> Option<Value> {
        let line_text = self.documents.line_text(uri, line).await?;
        let has_sorry = contains_word(&line_text, "sorry");
        let has_admit = contains_word(&line_text, "admit");
        if !has_sorry && !has_admit {
            return None;
        }
        let line_len = self.documents.line_length_utf16(uri, line).await?;
        let label = if has_sorry {
            " ⚠ sorry"
        } else {
            " ⚠ admit"
        };
        Some(json!({
            "position": { "line": line, "character": line_len },
            "label": label,
            "paddingLeft": true,
            "tooltip": {
                "kind": "markdown",
                "value": "**Admitted proof.** This declaration uses `sorry`/`admit`; the goal is not actually proven.",
            }
        }))
    }

    async fn probe(&self, uri: &str, line: u64, character: u64) -> Option<Value> {
        let current_version = self.state.version_for(uri).await.unwrap_or(0);

        if let Some(entry) = self
            .cache
            .lock()
            .await
            .get(uri)
            .and_then(|f| f.get(&(line, character)))
            .cloned()
            && entry.version == current_version
        {
            return entry.value;
        }

        let was_processing = self.state.is_processing(uri, line).await;

        let id = self.lsp.alloc_id();
        let params = json!({
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
        });
        let result = self
            .lsp
            .request_with_id(&id, "$/lean/plainGoal", params)
            .await
            .ok();
        let value = result.and_then(|v| if v.is_null() { None } else { Some(v) });

        if !was_processing {
            self.cache
                .lock()
                .await
                .entry(uri.to_string())
                .or_default()
                .insert(
                    (line, character),
                    CachedProbe {
                        version: current_version,
                        value: value.clone(),
                    },
                );
        }
        value
    }
}

fn build_hint(
    line: u64,
    character: u64,
    prev: Option<&Value>,
    curr: Option<&Value>,
    diagnostics: &[Value],
) -> Option<Value> {
    let prev_goals = prev.and_then(extract_goal_strs);
    let curr_goals = curr.and_then(extract_goal_strs);

    let goal_label = compute_label(prev_goals.as_deref(), curr_goals.as_deref());
    let error_marker = error_marker_for(diagnostics);

    let label = match (goal_label, error_marker) {
        (None, None) => return None,
        (Some(g), None) => g,
        (None, Some(e)) => e,
        (Some(g), Some(e)) => format!("{e}{g}"),
    };

    let goal_tooltip = curr
        .or(prev)
        .and_then(|c| c.get("rendered"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let diag_tooltip = render_diagnostic_tooltip(diagnostics);

    let tooltip = match (goal_tooltip.is_empty(), diag_tooltip.is_empty()) {
        (true, true) => String::new(),
        (false, true) => goal_tooltip,
        (true, false) => diag_tooltip,
        (false, false) => format!("{diag_tooltip}\n\n---\n\n{goal_tooltip}"),
    };

    Some(json!({
        "position": { "line": line, "character": character },
        "label": label,
        "paddingLeft": true,
        "tooltip": {
            "kind": "markdown",
            "value": tooltip,
        }
    }))
}

fn error_marker_for(diagnostics: &[Value]) -> Option<String> {
    let max = diagnostics
        .iter()
        .filter_map(|d| d.get("severity").and_then(Value::as_i64))
        .min()?;
    match max {
        v if v == SEVERITY_ERROR => {
            let unsolved = diagnostics.iter().any(|d| {
                d.get("message")
                    .and_then(Value::as_str)
                    .is_some_and(|m| m.starts_with("unsolved goals"))
            });
            if unsolved {
                Some(" ⚠ unsolved".to_string())
            } else {
                Some(" ✗".to_string())
            }
        }
        v if v == SEVERITY_WARNING => Some(" ⚠".to_string()),
        _ => None,
    }
}

fn render_diagnostic_tooltip(diagnostics: &[Value]) -> String {
    let messages: Vec<String> = diagnostics
        .iter()
        .filter_map(|d| {
            let sev = d.get("severity").and_then(Value::as_i64).unwrap_or(0);
            let prefix = match sev {
                v if v == SEVERITY_ERROR => "**error**: ",
                v if v == SEVERITY_WARNING => "**warning**: ",
                _ => "",
            };
            let msg = d.get("message").and_then(Value::as_str)?;
            Some(format!("{prefix}{msg}"))
        })
        .collect();
    messages.join("\n\n")
}

fn contains_word(haystack: &str, needle: &str) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let abs = start + idx;
        let before_ok = abs == 0
            || !haystack
                .as_bytes()
                .get(abs - 1)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        let after_idx = abs + needle.len();
        let after_ok = after_idx >= haystack.len()
            || !haystack
                .as_bytes()
                .get(after_idx)
                .is_some_and(|b| b.is_ascii_alphanumeric() || *b == b'_');
        if before_ok && after_ok {
            return true;
        }
        start = abs + needle.len();
    }
    false
}

fn extract_goal_strs(plain_goal: &Value) -> Option<Vec<&str>> {
    let arr = plain_goal.get("goals")?.as_array()?;
    let strs: Vec<&str> = arr.iter().filter_map(Value::as_str).collect();
    if strs.is_empty() { None } else { Some(strs) }
}

fn compute_label(prev: Option<&[&str]>, curr: Option<&[&str]>) -> Option<String> {
    let prev_targets = targets(prev);
    let curr_targets = targets(curr);
    let prev_n = prev_targets.len();
    let curr_n = curr_targets.len();

    if prev_n > 0 && curr_n == 0 {
        return Some(" ✓".to_string());
    }
    if curr_n == 0 {
        return None;
    }
    if prev_targets == curr_targets {
        let added = added_hyps(prev, curr);
        if added.is_empty() {
            return None;
        }
        return Some(format!(" + {}", added.join(", ")));
    }

    let prefix = if prev_n > 0 { "→ ⊢" } else { "⊢" };
    if curr_n == 1 {
        let target = &curr_targets[0];
        let len = target.chars().count();
        if len <= INLINE_TARGET_LIMIT {
            return Some(format!(" {prefix} {target}"));
        }
        let cutoff = INLINE_TARGET_LIMIT.saturating_sub(1);
        let truncated: String = target.chars().take(cutoff).collect();
        return Some(format!(" {prefix} {truncated}…"));
    }
    Some(format!(
        " {} {}",
        curr_n,
        if curr_n == 1 { "goal" } else { "goals" }
    ))
}

fn targets(goals: Option<&[&str]>) -> Vec<String> {
    goals
        .map(|g| g.iter().map(|s| extract_target(s)).collect())
        .unwrap_or_default()
}

fn added_hyps(prev: Option<&[&str]>, curr: Option<&[&str]>) -> Vec<String> {
    let prev_hyps = prev
        .and_then(|g| g.first().copied())
        .map(hyp_names)
        .unwrap_or_default();
    let curr_hyps = curr
        .and_then(|g| g.first().copied())
        .map(hyp_names)
        .unwrap_or_default();
    curr_hyps
        .into_iter()
        .filter(|n| !prev_hyps.contains(n))
        .collect()
}

fn hyp_names(goal: &str) -> Vec<String> {
    goal.lines()
        .take_while(|l| !l.starts_with('⊢'))
        .flat_map(|l| {
            let trimmed = l.trim();
            if trimmed.is_empty() || trimmed.starts_with("case ") {
                return Vec::new();
            }
            let Some(before_colon) = trimmed.split(':').next() else {
                return Vec::new();
            };
            before_colon
                .split_whitespace()
                .map(str::to_string)
                .collect()
        })
        .collect()
}

fn extract_target(goal: &str) -> String {
    goal.lines().find(|l| l.starts_with('⊢')).map_or_else(
        || goal.to_string(),
        |l| l.trim_start_matches('⊢').trim().to_string(),
    )
}
