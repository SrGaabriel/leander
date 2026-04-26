use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use serde_json::{Value, json};
use tokio::{
    sync::{broadcast, oneshot},
    task::JoinHandle,
};

use crate::{
    lsp::{LspHandle, ResponseError},
    state::{StateEvent, StateHandle},
};

const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
const INTERACTIVE_GOALS_METHOD: &str = "Lean.Widget.getInteractiveGoals";

pub struct RpcManager {
    lsp: LspHandle,
    sessions: Mutex<HashMap<String, SessionState>>,
    refs: Mutex<HashMap<String, (String, Vec<u64>)>>,
}

enum SessionState {
    Connecting(Vec<oneshot::Sender<Result<String, ResponseError>>>),
    Ready {
        session_id: String,
        _keepalive: KeepAliveGuard,
    },
}

struct KeepAliveGuard(JoinHandle<()>);

impl Drop for KeepAliveGuard {
    fn drop(&mut self) {
        self.0.abort();
    }
}

impl RpcManager {
    pub fn new(lsp: LspHandle, state: &StateHandle) -> Arc<Self> {
        let me = Arc::new(Self {
            lsp,
            sessions: Mutex::new(HashMap::new()),
            refs: Mutex::new(HashMap::new()),
        });
        spawn_event_watcher(me.clone(), state.subscribe());
        me
    }

    pub fn lsp(&self) -> &LspHandle {
        &self.lsp
    }

    pub async fn call_at(
        &self,
        uri: &str,
        line: u64,
        character: u64,
        id: &str,
    ) -> Result<Value, ResponseError> {
        let session_id = self.session(uri).await?;

        let params = json!({
            "sessionId": session_id,
            "textDocument": { "uri": uri },
            "position": { "line": line, "character": character },
            "method": INTERACTIVE_GOALS_METHOD,
            "params": {
                "textDocument": { "uri": uri },
                "position": { "line": line, "character": character },
            },
        });
        let result = self
            .lsp
            .request_with_id(id, "$/lean/rpc/call", params)
            .await?;

        let mut new_refs = Vec::new();
        collect_refs(&result, &mut new_refs);
        self.replace_refs(uri.to_string(), session_id, new_refs)
            .await;

        Ok(result)
    }

    async fn session(&self, uri: &str) -> Result<String, ResponseError> {
        enum Outcome {
            Have(String),
            Wait(oneshot::Receiver<Result<String, ResponseError>>),
            Connect,
        }
        let outcome = {
            let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
            match sessions.get_mut(uri) {
                Some(SessionState::Ready { session_id, .. }) => Outcome::Have(session_id.clone()),
                Some(SessionState::Connecting(waiters)) => {
                    let (tx, rx) = oneshot::channel();
                    waiters.push(tx);
                    Outcome::Wait(rx)
                }
                None => {
                    sessions.insert(uri.to_string(), SessionState::Connecting(Vec::new()));
                    Outcome::Connect
                }
            }
        };
        match outcome {
            Outcome::Have(s) => Ok(s),
            Outcome::Wait(rx) => rx.await.unwrap_or_else(|_| {
                Err(ResponseError {
                    code: -1,
                    message: "session waiter dropped".into(),
                })
            }),
            Outcome::Connect => self.do_connect(uri).await,
        }
    }

    async fn do_connect(&self, uri: &str) -> Result<String, ResponseError> {
        let id = self.lsp.alloc_id();
        let result = self
            .lsp
            .request_with_id(&id, "$/lean/rpc/connect", json!({ "uri": uri }))
            .await;

        let outcome: Result<String, ResponseError> = result.and_then(|v| {
            v.get("sessionId")
                .and_then(Value::as_str)
                .map(String::from)
                .ok_or(ResponseError {
                    code: -1,
                    message: "missing sessionId in $/lean/rpc/connect response".into(),
                })
        });

        let waiters = {
            let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
            match &outcome {
                Ok(session_id) => {
                    let keepalive = self.spawn_keepalive(uri.to_string(), session_id.clone());
                    let prev = sessions.insert(
                        uri.to_string(),
                        SessionState::Ready {
                            session_id: session_id.clone(),
                            _keepalive: KeepAliveGuard(keepalive),
                        },
                    );
                    take_waiters(prev)
                }
                Err(_) => take_waiters(sessions.remove(uri)),
            }
        };
        for w in waiters {
            let _ = w.send(outcome.clone());
        }
        outcome
    }

    fn spawn_keepalive(&self, uri: String, session_id: String) -> JoinHandle<()> {
        let lsp = self.lsp.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(KEEPALIVE_INTERVAL);
            interval.tick().await;
            loop {
                interval.tick().await;
                let r = lsp
                    .notify(
                        "$/lean/rpc/keepAlive",
                        json!({ "uri": uri, "sessionId": session_id }),
                    )
                    .await;
                if r.is_err() {
                    return;
                }
            }
        })
    }

    async fn replace_refs(&self, uri: String, session_id: String, new_refs: Vec<u64>) {
        let prev = {
            let mut buckets = self.refs.lock().expect("refs mutex poisoned");
            buckets.insert(uri.clone(), (session_id, new_refs))
        };
        if let Some((prev_session, prev_refs)) = prev
            && !prev_refs.is_empty()
        {
            let _ = self
                .lsp
                .notify(
                    "$/lean/rpc/release",
                    json!({
                        "uri": uri,
                        "sessionId": prev_session,
                        "refs": prev_refs,
                    }),
                )
                .await;
        }
    }

    fn handle_did_close(&self, uri: &str) {
        let removed = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .remove(uri);
        drop(removed);
        self.refs.lock().expect("refs mutex poisoned").remove(uri);
    }
}

fn take_waiters(
    state: Option<SessionState>,
) -> Vec<oneshot::Sender<Result<String, ResponseError>>> {
    match state {
        Some(SessionState::Connecting(waiters)) => waiters,
        _ => Vec::new(),
    }
}

fn spawn_event_watcher(rpc: Arc<RpcManager>, mut events: broadcast::Receiver<StateEvent>) {
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(StateEvent::DidClose { uri }) => rpc.handle_did_close(&uri),
                Ok(
                    StateEvent::ProgressChanged { .. }
                    | StateEvent::DidOpen { .. }
                    | StateEvent::DiagnosticsChanged { .. },
                )
                | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    });
}

fn collect_refs(v: &Value, out: &mut Vec<u64>) {
    match v {
        Value::Object(map) => {
            if let Some(n) = map.get("p").and_then(Value::as_u64) {
                out.push(n);
            }
            if let Some(n) = map.get("__rpcref").and_then(Value::as_u64) {
                out.push(n);
            }
            for val in map.values() {
                collect_refs(val, out);
            }
        }
        Value::Array(arr) => {
            for val in arr {
                collect_refs(val, out);
            }
        }
        _ => {}
    }
}
