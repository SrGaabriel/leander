use std::{sync::Arc, time::Duration};

use tokio::sync::{broadcast, watch};

use crate::{
    lean_rpc::RpcManager,
    lsp::ERROR_CANCELLED,
    snoop::CursorPos,
    state::{StateEvent, StateHandle},
};

const DEBOUNCE: Duration = Duration::from_millis(120);

pub fn spawn(
    rpc: Arc<RpcManager>,
    cursor_rx: watch::Receiver<Option<CursorPos>>,
    state: StateHandle,
) {
    tokio::spawn(run(rpc, cursor_rx, state));
}

async fn run(
    rpc: Arc<RpcManager>,
    mut cursor_rx: watch::Receiver<Option<CursorPos>>,
    state: StateHandle,
) {
    rpc.lsp().wait_initialized().await;
    let _ = cursor_rx.borrow_and_update();

    let mut events = state.subscribe();
    let mut prev_id: Option<String> = None;
    let mut prev_handle: Option<tokio::task::JoinHandle<()>> = None;
    let mut last_fired: Option<CursorPos> = None;

    loop {
        let signal = tokio::select! {
            r = cursor_rx.changed() => match r {
                Ok(()) => {
                    settle_cursor(&mut cursor_rx).await;
                    Signal::Cursor
                }
                Err(_) => return,
            },
            r = events.recv() => match r {
                Ok(StateEvent::ProgressChanged { uri }) => Signal::Progress(uri),
                Ok(StateEvent::DidClose { uri }) => Signal::DidClose(uri),
                Ok(StateEvent::DidOpen { .. } | StateEvent::DiagnosticsChanged { .. })
                | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return,
            },
        };

        let pos = cursor_rx.borrow().clone();
        let Some(pos) = pos else { continue };

        if let Signal::DidClose(uri) = &signal
            && pos.uri == *uri
        {
            cancel_inflight(&rpc, &mut prev_id, &mut prev_handle).await;
            last_fired = None;
            continue;
        }

        if let Signal::Progress(uri) = &signal
            && pos.uri != *uri
        {
            continue;
        }

        if state.is_processing(&pos.uri, pos.line) {
            continue;
        }

        if !matches!(signal, Signal::Cursor) && last_fired.as_ref() == Some(&pos) {
            continue;
        }

        cancel_inflight(&rpc, &mut prev_id, &mut prev_handle).await;

        let id = rpc.lsp().alloc_id();
        prev_id = Some(id.clone());
        last_fired = Some(pos.clone());

        let rpc = rpc.clone();
        let state = state.clone();
        prev_handle = Some(tokio::spawn(async move {
            match rpc.call_at(&pos.uri, pos.line, pos.character, &id).await {
                Ok(result) => {
                    state.update_goals(&pos.uri, pos.line, pos.character, result);
                }
                Err(e) if e.code == ERROR_CANCELLED => {}
                Err(e) => {
                    eprintln!("[goals] error {} {}", e.code, e.message);
                }
            }
        }));
    }
}

enum Signal {
    Cursor,
    Progress(String),
    DidClose(String),
}

async fn cancel_inflight(
    rpc: &Arc<RpcManager>,
    prev_id: &mut Option<String>,
    prev_handle: &mut Option<tokio::task::JoinHandle<()>>,
) {
    if let Some(prev) = prev_id.take() {
        rpc.lsp().cancel(&prev).await;
    }
    if let Some(h) = prev_handle.take() {
        h.abort();
    }
}

async fn settle_cursor(rx: &mut watch::Receiver<Option<CursorPos>>) {
    loop {
        tokio::select! {
            () = tokio::time::sleep(DEBOUNCE) => break,
            r = rx.changed() => if r.is_err() { break; },
        }
    }
    let _ = rx.borrow_and_update();
}
