use std::{collections::HashMap, time::Instant};

use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::{
    documents::Documents,
    lsp::LspHandle,
    state::{StateEvent, StateHandle},
};

const COOLDOWN_SECS: u64 = 10;
const LANGUAGE_ID: &str = "lean4";

pub fn spawn(lsp: LspHandle, state: StateHandle, documents: Documents) {
    tokio::spawn(run(lsp, state, documents));
}

async fn run(lsp: LspHandle, state: StateHandle, documents: Documents) {
    let mut events = state.subscribe();
    let mut last_restart: HashMap<String, Instant> = HashMap::new();
    loop {
        match events.recv().await {
            Ok(StateEvent::DiagnosticsChanged { uri }) => {
                maybe_restart(&lsp, &state, &documents, &uri, &mut last_restart).await;
            }
            Ok(StateEvent::DidClose { uri }) => {
                last_restart.remove(&uri);
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

async fn maybe_restart(
    lsp: &LspHandle,
    state: &StateHandle,
    documents: &Documents,
    uri: &str,
    last_restart: &mut HashMap<String, Instant>,
) {
    if let Some(t) = last_restart.get(uri)
        && t.elapsed().as_secs() < COOLDOWN_SECS
    {
        return;
    }

    let diagnostics = state.diagnostics_for(uri).await;
    let needs_restart = diagnostics.iter().any(|d| {
        d.get("message")
            .and_then(Value::as_str)
            .is_some_and(is_outdated_imports)
    });
    if !needs_restart {
        return;
    }

    let Some(text) = documents.full_text(uri).await else {
        return;
    };
    let version = state.version_for(uri).await.unwrap_or(0);

    eprintln!("[restart] auto-restarting {uri} (outdated imports)");
    last_restart.insert(uri.to_string(), Instant::now());

    let _ = lsp
        .notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": uri } }),
        )
        .await;
    let _ = lsp
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": LANGUAGE_ID,
                    "version": version,
                    "text": text,
                }
            }),
        )
        .await;
}

fn is_outdated_imports(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    lower.contains("import")
        && (lower.contains("outdated")
            || lower.contains("out of date")
            || lower.contains("restart"))
}
