use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, watch},
};

use crate::{
    framing::{encode_frame, read_message},
    snoop::{self, CursorPos},
    state::StateHandle,
};

const ID_PREFIX: &str = "leanto:";

pub const ERROR_CANCELLED: i64 = -32800;

#[derive(Clone, Debug)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, ResponseError>>>;

#[derive(Clone)]
pub struct LspHandle {
    inner: Arc<Inner>,
}

struct Inner {
    to_server: mpsc::Sender<Vec<u8>>,
    next_id: AtomicU64,
    pending: Arc<Mutex<Pending>>,
    init_rx: watch::Receiver<bool>,
}

impl LspHandle {
    pub fn alloc_id(&self) -> String {
        let n = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        format!("{ID_PREFIX}{n}")
    }

    pub async fn request_with_id(
        &self,
        id: &str,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        let (tx, rx) = oneshot::channel();
        self.inner
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id.to_string(), tx);

        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let frame = encode_frame(&serde_json::to_vec(&body).expect("serialize request"));

        if self.inner.to_server.send(frame).await.is_err() {
            self.inner
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(id);
            return Err(ResponseError {
                code: -1,
                message: "server pipe closed".into(),
            });
        }
        rx.await.unwrap_or_else(|_| {
            Err(ResponseError {
                code: -1,
                message: "router dropped".into(),
            })
        })
    }

    pub async fn notify(&self, method: &str, params: Value) -> std::io::Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let frame = encode_frame(&serde_json::to_vec(&body).expect("serialize notification"));
        self.inner
            .to_server
            .send(frame)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "server pipe closed"))
    }

    pub async fn cancel(&self, id: &str) {
        let _ = self.notify("$/cancelRequest", json!({ "id": id })).await;
        let waiter = self
            .inner
            .pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(id);
        if let Some(tx) = waiter {
            let _ = tx.send(Err(ResponseError {
                code: ERROR_CANCELLED,
                message: "cancelled by proxy".into(),
            }));
        }
    }

    pub async fn wait_initialized(&self) {
        let mut rx = self.inner.init_rx.clone();
        loop {
            if *rx.borrow() {
                return;
            }
            if rx.changed().await.is_err() {
                return;
            }
        }
    }
}

pub fn spawn(
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    cursor_tx: watch::Sender<Option<CursorPos>>,
    state: StateHandle,
) -> (LspHandle, tokio::task::JoinHandle<()>) {
    let (to_server_tx, to_server_rx) = mpsc::channel::<Vec<u8>>(64);
    let (init_tx, init_rx) = watch::channel(false);
    let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(Pending::new()));

    let inner = Arc::new(Inner {
        to_server: to_server_tx.clone(),
        next_id: AtomicU64::new(0),
        pending: pending.clone(),
        init_rx,
    });

    tokio::spawn(writer_task(child_stdin, to_server_rx));
    let c2s = tokio::spawn(c2s_task(to_server_tx, cursor_tx, init_tx));
    tokio::spawn(s2c_task(child_stdout, pending, state));

    (LspHandle { inner }, c2s)
}

async fn writer_task(mut child_stdin: ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if child_stdin.write_all(&frame).await.is_err() {
            break;
        }
        if child_stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn c2s_task(
    to_server: mpsc::Sender<Vec<u8>>,
    cursor_tx: watch::Sender<Option<CursorPos>>,
    init_tx: watch::Sender<bool>,
) {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    loop {
        match read_message(&mut reader).await {
            Ok(Some((hdr, body))) => {
                if let Some(pos) = snoop::extract_cursor(&body) {
                    let _ = cursor_tx.send(Some(pos));
                }
                if snoop::is_initialized(&body) {
                    let _ = init_tx.send(true);
                }
                let mut frame = hdr;
                frame.extend_from_slice(&body);
                if to_server.send(frame).await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("[proxy] c2s read error: {e}");
                break;
            }
        }
    }
}

async fn s2c_task(child_stdout: ChildStdout, pending: Arc<Mutex<Pending>>, state: StateHandle) {
    let mut reader = BufReader::new(child_stdout);
    let mut zed_stdout = tokio::io::stdout();
    loop {
        match read_message(&mut reader).await {
            Ok(Some((hdr, body))) => {
                let parsed: Option<Value> = serde_json::from_slice(&body).ok();

                if let Some(ref v) = parsed
                    && let Some(id) = v.get("id").and_then(Value::as_str)
                    && id.starts_with(ID_PREFIX)
                {
                    let waiter = pending.lock().expect("pending mutex poisoned").remove(id);
                    if let Some(tx) = waiter {
                        let result = if let Some(err) = v.get("error") {
                            let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
                            let msg = err
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            Err(ResponseError { code, message: msg })
                        } else {
                            Ok(v.get("result").cloned().unwrap_or(Value::Null))
                        };
                        let _ = tx.send(result);
                    }
                    continue;
                }

                if let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str) == Some("$/lean/fileProgress")
                    && let Some(params) = v.get("params")
                    && let Some(uri) = params.pointer("/textDocument/uri").and_then(Value::as_str)
                {
                    state.update_progress(uri.to_string(), params.clone()).await;
                }

                if zed_stdout.write_all(&hdr).await.is_err() {
                    break;
                }
                if zed_stdout.write_all(&body).await.is_err() {
                    break;
                }
                if zed_stdout.flush().await.is_err() {
                    break;
                }
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("[proxy] s2c read error: {e}");
                break;
            }
        }
    }
    let mut pending = pending.lock().expect("pending mutex poisoned");
    for (_, tx) in pending.drain() {
        let _ = tx.send(Err(ResponseError {
            code: -1,
            message: "server connection closed".into(),
        }));
    }
}
