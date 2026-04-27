#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_precision_loss)]

use std::collections::HashMap;

use serde_json::json;
use tokio::sync::broadcast;

use crate::{
    documents::Documents,
    lsp::LspHandle,
    state::{StateEvent, StateHandle},
};

pub fn spawn(lsp: LspHandle, state: StateHandle, documents: Documents) {
    tokio::spawn(run(lsp, state, documents));
}

async fn run(lsp: LspHandle, state: StateHandle, documents: Documents) {
    let mut events = state.subscribe();
    let mut active: HashMap<String, String> = HashMap::new();
    loop {
        match events.recv().await {
            Ok(StateEvent::ProgressChanged { uri }) => {
                handle_progress(&lsp, &state, &documents, &mut active, &uri).await;
            }
            Ok(StateEvent::DidClose { uri }) => {
                if let Some(token) = active.remove(&uri) {
                    end_progress(&lsp, &token).await;
                }
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn handle_progress(
    lsp: &LspHandle,
    state: &StateHandle,
    documents: &Documents,
    active: &mut HashMap<String, String>,
    uri: &str,
) {
    if !state.config().progress {
        if let Some(token) = active.remove(uri) {
            end_progress(lsp, &token).await;
        }
        return;
    }

    let frontier = state.elaboration_frontier(uri);
    let total_lines = documents.line_count(uri).unwrap_or(0);

    if frontier == u64::MAX {
        if let Some(token) = active.remove(uri) {
            end_progress(lsp, &token).await;
        }
        return;
    }

    let percent = compute_percent(frontier, total_lines);

    if let Some(token) = active.get(uri) {
        let _ = lsp
            .notify_client(
                "$/progress",
                json!({
                    "token": token,
                    "value": {
                        "kind": "report",
                        "percentage": percent,
                        "message": format!("line {} of {}", frontier, total_lines),
                    }
                }),
            )
            .await;
    } else {
        let token = format!("leanto-elaborate-{}", token_for(uri));
        active.insert(uri.to_string(), token.clone());
        let lsp = lsp.clone();
        let total = total_lines;
        let frontier_snap = frontier;
        tokio::spawn(async move {
            let _ = lsp
                .request_client("window/workDoneProgress/create", json!({ "token": &token }))
                .await;
            let _ = lsp
                .notify_client(
                    "$/progress",
                    json!({
                        "token": &token,
                        "value": {
                            "kind": "begin",
                            "title": "Elaborating Lean",
                            "percentage": compute_percent(frontier_snap, total),
                            "cancellable": false,
                        }
                    }),
                )
                .await;
        });
    }
}

async fn end_progress(lsp: &LspHandle, token: &str) {
    let _ = lsp
        .notify_client(
            "$/progress",
            json!({
                "token": token,
                "value": { "kind": "end", "message": "Done" },
            }),
        )
        .await;
}

fn compute_percent(frontier: u64, total_lines: u64) -> u32 {
    if total_lines == 0 || frontier == u64::MAX {
        return 100;
    }
    let pct = (frontier as f64 / total_lines as f64 * 100.0).clamp(0.0, 99.0);
    pct as u32
}

fn token_for(uri: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut h = DefaultHasher::new();
    uri.hash(&mut h);
    format!("{:x}", h.finish())
}
