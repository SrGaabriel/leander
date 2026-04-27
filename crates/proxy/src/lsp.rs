use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncWriteExt, BufReader, Stdout},
    process::{ChildStdin, ChildStdout},
    sync::{mpsc, oneshot, watch},
};

use crate::{
    documents::Documents,
    framing::{encode_frame, read_message, write_header},
    snoop::{self, CursorPos},
    state::{Config, StateHandle},
};

const ID_PREFIX: &str = "leanto:";

pub const ERROR_CANCELLED: i64 = -32800;

#[derive(Clone, Debug)]
pub struct ResponseError {
    pub code: i64,
    pub message: String,
}

type Pending = HashMap<String, oneshot::Sender<Result<Value, ResponseError>>>;

#[derive(Debug)]
pub struct InlayRequest {
    pub id: Value,
    pub params: Value,
}

#[derive(Debug)]
pub struct CodeLensRequest {
    pub id: Value,
    pub params: Value,
}

#[derive(Debug)]
pub struct SemanticTokensRequest {
    pub id: Value,
    pub params: Value,
}

#[derive(Clone)]
pub struct LspHandle {
    inner: Arc<Inner>,
}

struct Inner {
    to_server: mpsc::Sender<Vec<u8>>,
    to_zed: mpsc::Sender<Vec<u8>>,
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
        self.send_request(&self.inner.to_server, id, method, params, "server")
            .await
    }

    pub async fn request_client(
        &self,
        method: &str,
        params: Value,
    ) -> Result<Value, ResponseError> {
        let id = self.alloc_id();
        self.send_request(&self.inner.to_zed, &id, method, params, "zed")
            .await
    }

    async fn send_request(
        &self,
        target: &mpsc::Sender<Vec<u8>>,
        id: &str,
        method: &str,
        params: Value,
        target_name: &'static str,
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

        if target.send(frame).await.is_err() {
            self.inner
                .pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(id);
            return Err(ResponseError {
                code: -1,
                message: format!("{target_name} pipe closed"),
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
        send_notification(&self.inner.to_server, method, params, "server").await
    }

    #[allow(dead_code)]
    pub async fn notify_client(&self, method: &str, params: Value) -> std::io::Result<()> {
        send_notification(&self.inner.to_zed, method, params, "zed").await
    }

    pub async fn respond_to_client(&self, id: Value, result: Value) -> std::io::Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": result,
        });
        let frame = encode_frame(&serde_json::to_vec(&body).expect("serialize response"));
        self.inner
            .to_zed
            .send(frame)
            .await
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "zed pipe closed"))
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

async fn send_notification(
    target: &mpsc::Sender<Vec<u8>>,
    method: &str,
    params: Value,
    target_name: &'static str,
) -> std::io::Result<()> {
    let body = json!({
        "jsonrpc": "2.0",
        "method": method,
        "params": params,
    });
    let frame = encode_frame(&serde_json::to_vec(&body).expect("serialize notification"));
    target.send(frame).await.map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            format!("{target_name} pipe closed"),
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub fn spawn(
    child_stdin: ChildStdin,
    child_stdout: ChildStdout,
    cursor_tx: watch::Sender<Option<CursorPos>>,
    state: StateHandle,
    documents: Documents,
    inlay_requests: mpsc::Sender<InlayRequest>,
    lens_requests: mpsc::Sender<CodeLensRequest>,
    semantic_requests: mpsc::Sender<SemanticTokensRequest>,
) -> (LspHandle, tokio::task::JoinHandle<()>) {
    let (to_server_tx, to_server_rx) = mpsc::channel::<Vec<u8>>(64);
    let (to_zed_tx, to_zed_rx) = mpsc::channel::<Vec<u8>>(64);
    let (init_tx, init_rx) = watch::channel(false);
    let pending: Arc<Mutex<Pending>> = Arc::new(Mutex::new(Pending::new()));

    let inner = Arc::new(Inner {
        to_server: to_server_tx.clone(),
        to_zed: to_zed_tx.clone(),
        next_id: AtomicU64::new(0),
        pending: pending.clone(),
        init_rx,
    });
    let handle = LspHandle {
        inner: inner.clone(),
    };

    tokio::spawn(server_writer_task(child_stdin, to_server_rx));
    tokio::spawn(zed_writer_task(tokio::io::stdout(), to_zed_rx));
    let c2s = tokio::spawn(c2s_task(
        to_server_tx,
        pending.clone(),
        cursor_tx,
        init_tx,
        state.clone(),
        documents,
        inlay_requests,
        lens_requests,
        semantic_requests,
        handle.clone(),
    ));
    tokio::spawn(s2c_task(child_stdout, to_zed_tx, pending, state));

    (handle, c2s)
}

async fn server_writer_task(mut child_stdin: ChildStdin, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if child_stdin.write_all(&frame).await.is_err() {
            break;
        }
        if child_stdin.flush().await.is_err() {
            break;
        }
    }
}

async fn zed_writer_task(mut stdout: Stdout, mut rx: mpsc::Receiver<Vec<u8>>) {
    while let Some(frame) = rx.recv().await {
        if stdout.write_all(&frame).await.is_err() {
            break;
        }
        if stdout.flush().await.is_err() {
            break;
        }
    }
}

#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_lines)]
async fn c2s_task(
    to_server: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<Pending>>,
    cursor_tx: watch::Sender<Option<CursorPos>>,
    init_tx: watch::Sender<bool>,
    state: StateHandle,
    documents: Documents,
    inlay_requests: mpsc::Sender<InlayRequest>,
    lens_requests: mpsc::Sender<CodeLensRequest>,
    semantic_requests: mpsc::Sender<SemanticTokensRequest>,
    handle: LspHandle,
) {
    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin);
    loop {
        match read_message(&mut reader).await {
            Ok(Some((hdr, body))) => {
                let parsed: Option<Value> = serde_json::from_slice(&body).ok();
                if let Some(ref v) = parsed
                    && let Some(id) = v.get("id").and_then(Value::as_str)
                    && id.starts_with(ID_PREFIX)
                    && v.get("method").is_none()
                {
                    let waiter = pending.lock().expect("pending mutex poisoned").remove(id);
                    if let Some(tx) = waiter {
                        let _ = tx.send(extract_result(v));
                    }
                    continue;
                }
                if let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str) == Some("initialize")
                    && let Some(opts) = v.pointer("/params/initializationOptions")
                {
                    state.set_config(Config::from_init_options(opts)).await;
                }

                let cfg = state.config().await;

                if cfg.inlay_hints
                    && let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str) == Some("textDocument/inlayHint")
                    && v.get("id").is_some()
                {
                    let id = v.get("id").cloned().unwrap_or(Value::Null);
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    let _ = inlay_requests.send(InlayRequest { id, params }).await;
                    continue;
                }
                if cfg.code_lens
                    && let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str) == Some("textDocument/codeLens")
                    && v.get("id").is_some()
                {
                    let id = v.get("id").cloned().unwrap_or(Value::Null);
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    let _ = lens_requests.send(CodeLensRequest { id, params }).await;
                    continue;
                }
                if cfg.semantic_tokens
                    && let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str)
                        == Some("textDocument/semanticTokens/full")
                    && v.get("id").is_some()
                {
                    let id = v.get("id").cloned().unwrap_or(Value::Null);
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    let _ = semantic_requests
                        .send(SemanticTokensRequest { id, params })
                        .await;
                    continue;
                }
                if cfg.hover
                    && let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str) == Some("textDocument/hover")
                    && let Some(id) = v.get("id").cloned()
                    && let Some(uri) = v
                        .pointer("/params/textDocument/uri")
                        .and_then(Value::as_str)
                    && let Some(line) = v.pointer("/params/position/line").and_then(Value::as_u64)
                    && let Some(character) = v
                        .pointer("/params/position/character")
                        .and_then(Value::as_u64)
                {
                    let handle = handle.clone();
                    let state = state.clone();
                    let uri = uri.to_string();
                    tokio::spawn(async move {
                        merge_hover(handle, state, id, uri, line, character).await;
                    });
                    continue;
                }
                if let Some(ref v) = parsed {
                    if let Some(pos) = snoop::extract_cursor(v) {
                        let _ = cursor_tx.send(Some(pos));
                    }
                    if snoop::is_initialized(v) {
                        let _ = init_tx.send(true);
                    }
                    if let Some(uri) = snoop::extract_did_open(v) {
                        state.note_did_open(uri);
                    }
                    if let Some(uri) = snoop::extract_did_close(v) {
                        state.note_did_close(uri).await;
                    }
                    if let Some((uri, ver)) = snoop::extract_did_open_version(v) {
                        state.update_version(uri, ver).await;
                    }
                    if let Some((uri, ver)) = snoop::extract_did_change(v) {
                        state.update_version(uri, ver).await;
                    }
                    documents.handle_message(v).await;
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

async fn s2c_task(
    child_stdout: ChildStdout,
    to_zed: mpsc::Sender<Vec<u8>>,
    pending: Arc<Mutex<Pending>>,
    state: StateHandle,
) {
    let mut reader = BufReader::new(child_stdout);
    loop {
        match read_message(&mut reader).await {
            Ok(Some((hdr, body))) => {
                let parsed: Option<Value> = serde_json::from_slice(&body).ok();
                if let Some(ref v) = parsed
                    && let Some(id) = v.get("id").and_then(Value::as_str)
                    && id.starts_with(ID_PREFIX)
                    && v.get("method").is_none()
                {
                    let waiter = pending.lock().expect("pending mutex poisoned").remove(id);
                    if let Some(tx) = waiter {
                        let _ = tx.send(extract_result(v));
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
                if let Some(ref v) = parsed
                    && v.get("method").and_then(Value::as_str)
                        == Some("textDocument/publishDiagnostics")
                    && let Some(params) = v.get("params")
                    && let Some(uri) = params.pointer("/uri").and_then(Value::as_str)
                {
                    let diags = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    state.update_diagnostics(uri.to_string(), diags).await;
                }

                if let Some(ref v) = parsed
                    && is_initialize_response(v)
                    && let Some(legend) =
                        v.pointer("/result/capabilities/semanticTokensProvider/legend")
                {
                    if let Some(types) = legend.get("tokenTypes").and_then(Value::as_array) {
                        let names: Vec<String> = types
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        state.set_semantic_token_types(names).await;
                    }
                    if let Some(mods) = legend.get("tokenModifiers").and_then(Value::as_array) {
                        let names: Vec<String> = mods
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect();
                        state.set_semantic_token_modifiers(names).await;
                    }
                }

                let cfg = state.config().await;
                let (out_hdr, out_body) = if let Some(ref v) = parsed
                    && is_initialize_response(v)
                    && let Some(modified) = maybe_inject_capabilities(v.clone(), &cfg)
                {
                    let new_body = serde_json::to_vec(&modified)
                        .expect("serialize modified initialize response");
                    let mut new_hdr = Vec::with_capacity(32);
                    write_header(&mut new_hdr, new_body.len());
                    (new_hdr, new_body)
                } else {
                    (hdr, body)
                };

                let mut frame = out_hdr;
                frame.extend_from_slice(&out_body);
                if to_zed.send(frame).await.is_err() {
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

fn extract_result(v: &Value) -> Result<Value, ResponseError> {
    if let Some(err) = v.get("error") {
        let code = err.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        Err(ResponseError { code, message })
    } else {
        Ok(v.get("result").cloned().unwrap_or(Value::Null))
    }
}

fn is_initialize_response(v: &Value) -> bool {
    v.pointer("/result/capabilities")
        .is_some_and(Value::is_object)
}

async fn merge_hover(
    handle: LspHandle,
    state: StateHandle,
    original_id: Value,
    uri: String,
    line: u64,
    character: u64,
) {
    use std::time::Duration;

    let hover_id = handle.alloc_id();
    let hover_params = json!({
        "textDocument": { "uri": &uri },
        "position": { "line": line, "character": character },
    });
    let hover_fut = handle.request_with_id(&hover_id, "textDocument/hover", hover_params);
    let frontier = state.elaboration_frontier(&uri).await;
    let in_progress = state.is_processing(&uri, line).await;
    let goal_worth_querying = line < frontier && !in_progress;

    let (hover_value, goal_value) = if goal_worth_querying {
        let goal_id = handle.alloc_id();
        let goal_params = json!({
            "textDocument": { "uri": &uri },
            "position": { "line": line, "character": character },
        });
        let goal_fut = handle.request_with_id(&goal_id, "$/lean/plainGoal", goal_params);
        let goal_with_timeout = tokio::time::timeout(Duration::from_millis(150), goal_fut);
        let (h, g) = tokio::join!(hover_fut, goal_with_timeout);
        let goal = match g {
            Ok(Ok(v)) => v,
            _ => Value::Null,
        };
        (h.unwrap_or(Value::Null), goal)
    } else {
        (hover_fut.await.unwrap_or(Value::Null), Value::Null)
    };

    let merged = merge_hover_value(hover_value, &goal_value);
    let _ = handle.respond_to_client(original_id, merged).await;
}

fn merge_hover_value(mut hover: Value, goal: &Value) -> Value {
    let goal_md = goal_to_markdown(goal);
    if goal_md.is_empty() {
        return hover;
    }

    let block = format!("\n\n---\n\n**Goal at this position:**\n\n{goal_md}");

    if hover.is_null() {
        return json!({
            "contents": { "kind": "markdown", "value": block.trim_start().to_string() }
        });
    }

    if let Some(obj) = hover.as_object_mut() {
        match obj.get_mut("contents") {
            Some(Value::Object(mc)) => {
                if let Some(Value::String(v)) = mc.get_mut("value") {
                    v.push_str(&block);
                }
                if !mc.contains_key("kind") {
                    mc.insert("kind".to_string(), json!("markdown"));
                }
            }
            Some(Value::String(s)) => {
                let combined = format!("{s}{block}");
                obj.insert(
                    "contents".to_string(),
                    json!({ "kind": "markdown", "value": combined }),
                );
            }
            Some(Value::Array(arr)) => {
                arr.push(json!({ "language": "markdown", "value": block }));
            }
            _ => {
                obj.insert(
                    "contents".to_string(),
                    json!({ "kind": "markdown", "value": block.trim_start().to_string() }),
                );
            }
        }
    }
    hover
}

fn goal_to_markdown(goal: &Value) -> String {
    if goal.is_null() {
        return String::new();
    }
    if let Some(rendered) = goal.get("rendered").and_then(Value::as_str)
        && !rendered.is_empty()
    {
        return rendered.to_string();
    }
    if let Some(arr) = goal.get("goals").and_then(Value::as_array) {
        let parts: Vec<String> = arr
            .iter()
            .filter_map(Value::as_str)
            .map(|g| format!("```lean\n{g}\n```"))
            .collect();
        return parts.join("\n\n");
    }
    String::new()
}

fn maybe_inject_capabilities(mut v: Value, cfg: &Config) -> Option<Value> {
    let caps = v.pointer_mut("/result/capabilities")?.as_object_mut()?;
    let mut changed = false;
    if cfg.inlay_hints && !caps.contains_key("inlayHintProvider") {
        caps.insert("inlayHintProvider".to_string(), json!(true));
        changed = true;
    }
    if cfg.code_lens && !caps.contains_key("codeLensProvider") {
        caps.insert(
            "codeLensProvider".to_string(),
            json!({ "resolveProvider": false }),
        );
        changed = true;
    }
    if changed { Some(v) } else { None }
}

#[allow(dead_code)]
fn maybe_inject_inlay_capability(mut v: Value) -> Option<Value> {
    let caps = v.pointer_mut("/result/capabilities")?.as_object_mut()?;
    let mut changed = false;
    if !caps.contains_key("inlayHintProvider") {
        caps.insert("inlayHintProvider".to_string(), json!(true));
        changed = true;
    }
    if !caps.contains_key("codeLensProvider") {
        caps.insert(
            "codeLensProvider".to_string(),
            json!({ "resolveProvider": false }),
        );
        changed = true;
    }
    if changed { Some(v) } else { None }
}
