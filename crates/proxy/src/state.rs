#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::{Mutex, broadcast};

#[derive(Clone, Debug)]
pub struct GoalSnapshot {
    pub uri: String,
    pub line: u64,
    pub character: u64,
    pub goals: Value,
}

#[derive(Clone, Debug)]
pub enum StateEvent {
    ProgressChanged { uri: String },
    DiagnosticsChanged { uri: String },
    DidOpen { uri: String },
    DidClose { uri: String },
}

pub const SEVERITY_ERROR: i64 = 1;
pub const SEVERITY_WARNING: i64 = 2;
pub const SEVERITY_INFO: i64 = 3;
pub const SEVERITY_HINT: i64 = 4;

#[derive(Clone, Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Config {
    pub inlay_hints: bool,
    pub code_lens: bool,
    pub semantic_tokens: bool,
    pub hover: bool,
    pub progress: bool,
    pub auto_restart: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            inlay_hints: true,
            code_lens: true,
            semantic_tokens: true,
            hover: true,
            progress: true,
            auto_restart: true,
        }
    }
}

impl Config {
    pub fn from_init_options(opts: &Value) -> Self {
        let leanto = opts.get("leanTo").or_else(|| opts.get("leanto"));
        let mut cfg = Self::default();
        let Some(leanto) = leanto else {
            return cfg;
        };
        let read =
            |k: &str, fallback: bool| leanto.get(k).and_then(Value::as_bool).unwrap_or(fallback);
        cfg.inlay_hints = read("inlayHints", cfg.inlay_hints);
        cfg.code_lens = read("codeLens", cfg.code_lens);
        cfg.semantic_tokens = read("semanticTokens", cfg.semantic_tokens);
        cfg.hover = read("hover", cfg.hover);
        cfg.progress = read("progress", cfg.progress);
        cfg.auto_restart = read("autoRestart", cfg.auto_restart);
        cfg
    }
}

#[derive(Clone)]
pub struct StateHandle {
    inner: Arc<Mutex<Inner>>,
    events: broadcast::Sender<StateEvent>,
}

#[derive(Default)]
struct Inner {
    goals: HashMap<String, GoalSnapshot>,
    progress: HashMap<String, Value>,
    versions: HashMap<String, i64>,
    diagnostics: HashMap<String, Vec<Value>>,
    semantic_token_types: Vec<String>,
    semantic_token_modifiers: Vec<String>,
    config: Config,
}

impl StateHandle {
    pub fn new() -> Self {
        let (events, _) = broadcast::channel(64);
        Self {
            inner: Arc::new(Mutex::new(Inner::default())),
            events,
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<StateEvent> {
        self.events.subscribe()
    }

    pub async fn update_goals(&self, uri: &str, line: u64, character: u64, goals: Value) {
        let snap = GoalSnapshot {
            uri: uri.to_string(),
            line,
            character,
            goals,
        };
        self.inner.lock().await.goals.insert(uri.to_string(), snap);
    }

    pub async fn update_progress(&self, uri: String, params: Value) {
        self.inner.lock().await.progress.insert(uri.clone(), params);
        let _ = self.events.send(StateEvent::ProgressChanged { uri });
    }

    pub fn note_did_open(&self, uri: String) {
        let _ = self.events.send(StateEvent::DidOpen { uri });
    }

    pub async fn note_did_close(&self, uri: String) {
        {
            let mut inner = self.inner.lock().await;
            inner.goals.remove(&uri);
            inner.progress.remove(&uri);
            inner.versions.remove(&uri);
        }
        let _ = self.events.send(StateEvent::DidClose { uri });
    }

    pub async fn update_version(&self, uri: String, version: i64) {
        self.inner.lock().await.versions.insert(uri, version);
    }

    pub async fn version_for(&self, uri: &str) -> Option<i64> {
        self.inner.lock().await.versions.get(uri).copied()
    }

    pub async fn goals_for(&self, uri: &str) -> Option<GoalSnapshot> {
        self.inner.lock().await.goals.get(uri).cloned()
    }

    pub async fn progress_for(&self, uri: &str) -> Option<Value> {
        self.inner.lock().await.progress.get(uri).cloned()
    }

    pub async fn update_diagnostics(&self, uri: String, diagnostics: Vec<Value>) {
        self.inner
            .lock()
            .await
            .diagnostics
            .insert(uri.clone(), diagnostics);
        let _ = self.events.send(StateEvent::DiagnosticsChanged { uri });
    }

    pub async fn diagnostics_for(&self, uri: &str) -> Vec<Value> {
        self.inner
            .lock()
            .await
            .diagnostics
            .get(uri)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn diagnostics_at_line(&self, uri: &str, line: u64) -> Vec<Value> {
        let inner = self.inner.lock().await;
        let Some(diags) = inner.diagnostics.get(uri) else {
            return Vec::new();
        };
        diags
            .iter()
            .filter(|d| diagnostic_covers_line(d, line))
            .cloned()
            .collect()
    }

    pub async fn is_processing(&self, uri: &str, line: u64) -> bool {
        let inner = self.inner.lock().await;
        let Some(progress) = inner.progress.get(uri) else {
            return false;
        };
        line_in_processing(progress, line)
    }

    pub async fn set_semantic_token_types(&self, types: Vec<String>) {
        self.inner.lock().await.semantic_token_types = types;
    }

    pub async fn set_semantic_token_modifiers(&self, modifiers: Vec<String>) {
        self.inner.lock().await.semantic_token_modifiers = modifiers;
    }

    #[allow(clippy::cast_possible_truncation)]
    pub async fn token_modifier_index(&self, name: &str) -> Option<u32> {
        let inner = self.inner.lock().await;
        inner
            .semantic_token_modifiers
            .iter()
            .position(|m| m == name)
            .map(|i| i as u32)
    }

    pub async fn set_config(&self, config: Config) {
        self.inner.lock().await.config = config;
    }

    pub async fn config(&self) -> Config {
        self.inner.lock().await.config.clone()
    }

    #[allow(clippy::cast_possible_truncation)]
    pub async fn token_type_index(&self, name: &str) -> Option<u32> {
        let inner = self.inner.lock().await;
        inner
            .semantic_token_types
            .iter()
            .position(|t| t == name)
            .map(|i| i as u32)
    }

    pub async fn elaboration_frontier(&self, uri: &str) -> u64 {
        let inner = self.inner.lock().await;
        let Some(progress) = inner.progress.get(uri) else {
            return u64::MAX;
        };
        let Some(arr) = progress.get("processing").and_then(Value::as_array) else {
            return u64::MAX;
        };
        if arr.is_empty() {
            return u64::MAX;
        }
        arr.iter()
            .filter_map(|item| item.pointer("/range/start/line").and_then(Value::as_u64))
            .min()
            .unwrap_or(u64::MAX)
    }
}

fn diagnostic_covers_line(diagnostic: &Value, line: u64) -> bool {
    let s = diagnostic
        .pointer("/range/start/line")
        .and_then(Value::as_u64);
    let e = diagnostic
        .pointer("/range/end/line")
        .and_then(Value::as_u64);
    matches!((s, e), (Some(s), Some(e)) if line >= s && line <= e)
}

fn line_in_processing(progress: &Value, line: u64) -> bool {
    let Some(arr) = progress.get("processing").and_then(Value::as_array) else {
        return false;
    };
    arr.iter().any(|item| {
        let s = item.pointer("/range/start/line").and_then(Value::as_u64);
        let e = item.pointer("/range/end/line").and_then(Value::as_u64);
        match (s, e) {
            (Some(s), Some(e)) => line >= s && line <= e,
            _ => false,
        }
    })
}
