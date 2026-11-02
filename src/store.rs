//! In-memory store with the TS `Store` shape (triggers, threads,
//! tickets, decisions, dispositions) plus JSON persistence for the CLI.
//! Transactions in the TS code are synchronous repo transactions; here
//! the Engine holds the store behind one Mutex in the CLI, and directly
//! in tests (single-threaded by construction).

use crate::model::*;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub triggers: HashMap<String, Trigger>,
    #[serde(default)]
    pub dispositions: HashMap<String, TriggerDisposition>,
    #[serde(default)]
    pub threads: HashMap<String, Thread>,
    #[serde(default)]
    pub tickets: HashMap<String, Ticket>,
    #[serde(default)]
    pub decisions: HashMap<String, Decision>,
    #[serde(default)]
    pub counter: u64,
}

impl Store {
    pub fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{}-{}", prefix, self.counter)
    }
    pub fn thread_or_create(&mut self, r: &ThreadRef, at: &str) -> &mut Thread {
        let key = thread_key(r);
        self.threads.entry(key.clone()).or_insert_with(|| Thread {
            key,
            r#ref: r.clone(),
            created_at: at.to_string(),
            last_at: None,
            subscriptions: vec![],
            context: None,
            correlated: vec![],
            pending: vec![],
            evaluating: None,
            expired: None,
            meta: serde_json::json!({}),
        })
    }
    /// Invariants checked by deep tests after every operation.
    pub fn check_invariants(&self) -> Vec<String> {
        let mut problems = vec![];
        for (key, th) in &self.threads {
            if th.key != *key {
                problems.push(format!("thread key mismatch {key}"));
            }
            // at most one eval per thread is structural (Option), but
            // pending must not overlap evaluating trigger ids.
            if let Some(ev) = &th.evaluating {
                for id in &ev.trigger_ids {
                    if th.pending.contains(id) {
                        problems.push(format!("trigger {id} both pending and evaluating on {key}"));
                    }
                }
            }
            for sub in &th.subscriptions {
                if !self.tickets.contains_key(sub) {
                    problems.push(format!("subscription to unknown ticket {sub} on {key}"));
                }
            }
        }
        for (id, t) in &self.tickets {
            for u in &t.undelivered {
                if !t.inputs.iter().any(|i| i.id() == u) {
                    problems.push(format!("ticket {id} undelivered {u} has no input"));
                }
            }
            let running = t.assignments.iter().filter(|a| a.state == "running").count();
            if running > 1 {
                problems.push(format!("ticket {id} has {running} running assignments"));
            }
            if t.ended.is_some() && running > 0 {
                problems.push(format!("ticket {id} ended but assignment running"));
            }
        }
        for (id, d) in &self.decisions {
            if !self.threads.contains_key(&d.thread_key) {
                problems.push(format!("decision {id} on unknown thread {}", d.thread_key));
            }
        }
        problems
    }
}
