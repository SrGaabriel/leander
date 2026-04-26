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
