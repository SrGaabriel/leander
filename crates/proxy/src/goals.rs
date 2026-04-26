use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;

use crate::{
    lsp::{ERROR_CANCELLED, LspHandle},
    snoop::CursorPos,
    state::StateHandle,
};

const DEBOUNCE: Duration = Duration::from_millis(120);

pub fn spawn(lsp: LspHandle, cursor_rx: watch::Receiver<Option<CursorPos>>, state: StateHandle) {
    tokio::spawn(run(lsp, cursor_rx, state));
}

async fn run(
    lsp: LspHandle,
    mut cursor_rx: watch::Receiver<Option<CursorPos>>,
    state: StateHandle,
) {
    lsp.wait_initialized().await;
    eprintln!("[goals] initialized; tracking cursor");

    let _ = cursor_rx.borrow_and_update();

    let mut prev_id: Option<String> = None;
    let mut prev_handle: Option<tokio::task::JoinHandle<()>> = None;

    loop {
        let Some(pos) = next_settled_cursor(&mut cursor_rx).await else {
            return;
        };

        if let Some(prev) = prev_id.take() {
            lsp.cancel(&prev).await;
        }
        if let Some(h) = prev_handle.take() {
            h.abort();
        }

        let id = lsp.alloc_id();
        prev_id = Some(id.clone());

        let lsp = lsp.clone();
        let state = state.clone();
        prev_handle = Some(tokio::spawn(async move {
            let params = json!({
                "textDocument": { "uri": &pos.uri },
                "position": { "line": pos.line, "character": pos.character },
            });
            match lsp.request_with_id(&id, "$/lean/plainGoal", params).await {
                Ok(result) => {
                    let goals = goals(&result);
                    eprintln!(
                        "[goals] {} {}:{} → {}",
                        pos.uri, pos.line, pos.character, goals
                    );
                    state
                        .update_goals(&pos.uri, pos.line, pos.character, result)
                        .await;
                }
                Err(e) if e.code == ERROR_CANCELLED => {}
                Err(e) => {
                    eprintln!("[goals] error {} {}", e.code, e.message);
                }
            }
        }));
    }
}

async fn next_settled_cursor(rx: &mut watch::Receiver<Option<CursorPos>>) -> Option<CursorPos> {
    loop {
        rx.changed().await.ok()?;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(DEBOUNCE) => break,
                r = rx.changed() => { r.ok()?; }
            }
        }
        if let Some(pos) = rx.borrow_and_update().clone() {
            return Some(pos);
        }
    }
}

fn goals(result: &Value) -> String {
    if result.is_null() {
        return "no goals".into();
    }
    let n = result.get("goals").and_then(Value::as_array);
    serde_json::to_string(&n).unwrap_or_else(|_| "invalid goals".into())
}
