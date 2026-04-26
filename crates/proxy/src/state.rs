#![allow(dead_code)]

use std::{collections::HashMap, sync::Arc};

use serde_json::Value;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct GoalSnapshot {
    pub uri: String,
    pub line: u64,
    pub character: u64,
    pub plain_goal: Value,
}

#[derive(Clone, Default)]
pub struct StateHandle {
    inner: Arc<Mutex<Inner>>,
}

#[derive(Default)]
struct Inner {
    goals: HashMap<String, GoalSnapshot>,
    file_progress: HashMap<String, Value>,
}

impl StateHandle {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn update_goals(&self, uri: &str, line: u64, character: u64, plain_goal: Value) {
        let snap = GoalSnapshot {
            uri: uri.to_string(),
            line,
            character,
            plain_goal,
        };
        self.inner.lock().await.goals.insert(uri.to_string(), snap);
    }

    pub async fn update_progress(&self, uri: String, params: Value) {
        self.inner.lock().await.file_progress.insert(uri, params);
    }

    pub async fn goals_for(&self, uri: &str) -> Option<GoalSnapshot> {
        self.inner.lock().await.goals.get(uri).cloned()
    }

    pub async fn progress_for(&self, uri: &str) -> Option<Value> {
        self.inner.lock().await.file_progress.get(uri).cloned()
    }
}
