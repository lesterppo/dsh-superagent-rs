//! Domain model, ported from `src/core/model.ts` of
//! deepseek-ai/dsh-superagent-prototype (MIT).
//!
//! Concepts kept exactly: Source/Trigger/Thread/Eval/Mandate/Ticket/
//! Assignment/Subscription, with the same invariants where they are
//! checkable without the DSH host (one Eval per thread, ticket state
//! owned by the ticket not the executor, follow-up ownership, etc.).
//! The React panel, DSH session APIs, and Feishu/GitHub live adapters
//! are deliberately NOT ported: they belong to a deployment, per
//! docs/001. The executor here is a local workdir subprocess host.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub fn thread_key(r: &ThreadRef) -> String {
    format!("{}:{}", r.source, r.id)
}

/// Normalize a timestamp to `Date#toISOString` form (RFC3339, millis, Z)
/// so stored timestamps compare lexicographically; unparseable values
/// are returned unchanged (mirrors TS `isoTime`).
pub fn iso_time(value: &str) -> String {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(value) {
        return dt
            .with_timezone(&chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
    }
    // GitHub whole-second stamps etc. also parse via a second attempt.
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return chrono::DateTime::<chrono::Utc>::from_naive_utc_and_offset(dt, chrono::Utc)
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string();
    }
    value.to_string()
}

pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub source: String,
    pub id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadRef {
    pub source: String,
    pub id: String,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelRef {
    pub source: String,
    pub thread: String,
    #[serde(default)]
    pub address: HashMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trigger {
    pub id: String,
    pub source: String,
    pub threads: Vec<ThreadRef>,
    pub actor: ActorRef,
    pub human: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addressed: Option<String>, // "mention" | "direct"
    pub at: String,
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<ChannelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Handback {
    pub ticket_id: String,
    pub reason: String,
    pub at: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TriggerDisposition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filtered: Option<Reason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eval_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    #[serde(default)]
    pub fanout: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requeued: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub handbacks: Option<Vec<Handback>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked: Option<Vec<String>>,
    pub at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reason {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub values: Option<HashMap<String, String>>,
}

pub fn said(text: &str) -> Reason {
    Reason {
        code: "said".to_string(),
        trigger_id: None,
        values: Some(HashMap::from([("text".to_string(), text.to_string())])),
    }
}

/// The tickets whose worker decides a person's follow-up (TS `followupOwners`).
/// `tickets`, `threads`, `dispositions` are the stored records.
pub fn followup_owners(
    trigger: &Trigger,
    tickets: &HashMap<String, Ticket>,
    threads: &HashMap<String, Thread>,
    dispositions: &HashMap<String, TriggerDisposition>,
) -> Vec<String> {
    if !trigger.human {
        return vec![];
    }
    let disp = match dispositions.get(&trigger.id) {
        Some(d) => d,
        None => return vec![],
    };
    if disp.requeued == Some(true) {
        return vec![];
    }
    let returned: std::collections::HashSet<&str> = disp
        .handbacks
        .as_ref()
        .map(|h| h.iter().map(|x| x.ticket_id.as_str()).collect())
        .unwrap_or_default();
    let context: std::collections::HashSet<&str> = disp
        .context
        .as_ref()
        .map(|c| c.iter().map(|x| x.as_str()).collect())
        .unwrap_or_default();
    let reached: Vec<&str> = disp
        .fanout
        .iter()
        .map(|s| s.as_str())
        .filter(|id| !returned.contains(id) && !context.contains(id))
        .collect();
    if reached.is_empty() {
        return vec![];
    }
    if trigger.threads.is_empty() {
        return vec![];
    }
    let primary_key = thread_key(&trigger.threads[0]);
    if let Some(th) = threads.get(&primary_key) {
        if let Some(ev) = &th.evaluating {
            if ev.trigger_ids.contains(&trigger.id) {
                return vec![];
            }
        }
    }
    reached
        .into_iter()
        .filter(|id| {
            let t = match tickets.get(*id) {
                Some(t) => t,
                None => return false,
            };
            live_ticket(t)
                || (t.ended.as_ref().map(|e| e.by.as_str()) != Some("human")
                    && t.inputs.iter().any(|inp| {
                        matches!(inp, TicketInput::Trigger { trigger_id, .. } if trigger_id == &trigger.id)
                            && t.undelivered.contains(
                                &match inp {
                                    TicketInput::Trigger { id, .. } => id.clone(),
                                    _ => String::new(),
                                },
                            )
                    }))
        })
        .map(|s| s.to_string())
        .collect()
}

/// Whether a Decision settled what a person's message asks for (TS `settled`).
pub fn settled(decision: &Decision) -> bool {
    if matches!(decision.kind.as_str(), "stop" | "unavailable") {
        return false;
    }
    !decision
        .reason
        .iter()
        .any(|r| matches!(r.code.as_str(), "stopNotExplicit" | "stopUnknown" | "routed"))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Strength {
    Explicit,
    Inferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AdmissionCell {
    #[serde(rename = "listedUnaddressed")]
    ListedUnaddressed,
    #[serde(rename = "unlistedAddressed")]
    UnlistedAddressed,
    #[serde(rename = "unlistedUnaddressed")]
    UnlistedUnaddressed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AcceptMode {
    Off,
    Observe,
    Auto,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "via")]
pub enum MandateBasis {
    #[serde(rename = "addressed")]
    Addressed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<String>>,
        addressed: String,
    },
    #[serde(rename = "followup")]
    Followup {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<String>>,
    },
    #[serde(rename = "admission")]
    Admission {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<String>>,
        cell: AdmissionCell,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addressed: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observation: Option<Observation>,
    },
    #[serde(rename = "listed")]
    Listed {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addressed: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observation: Option<Observation>,
    },
    #[serde(rename = "bot")]
    Bot {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rules: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        addressed: Option<String>,
        observation: Observation,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Observation {
    pub mode: AcceptMode,
}

impl MandateBasis {
    pub fn source(&self) -> Option<&str> {
        match self {
            MandateBasis::Addressed { source, .. }
            | MandateBasis::Followup { source, .. }
            | MandateBasis::Admission { source, .. }
            | MandateBasis::Listed { source, .. }
            | MandateBasis::Bot { source, .. } => source.as_deref(),
        }
    }
    pub fn rules(&self) -> Option<&Vec<String>> {
        match self {
            MandateBasis::Addressed { rules, .. }
            | MandateBasis::Followup { rules, .. }
            | MandateBasis::Admission { rules, .. }
            | MandateBasis::Listed { rules, .. }
            | MandateBasis::Bot { rules, .. } => rules.as_ref(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mandate {
    pub strength: Strength,
    pub policy_version: String,
    pub basis: MandateBasis,
    pub actor: ActorRef,
    pub scope: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Evaluation {
    pub id: String,
    pub thread_key: String,
    pub trigger_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary: Option<String>,
    pub strength: Strength,
    pub actor: ActorRef,
    pub basis: MandateBasis,
    pub policy_version: String,
    pub started_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered: Option<String>,
    pub deadline: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paused_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub key: String,
    pub r#ref: ThreadRef,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_at: Option<String>,
    #[serde(default)]
    pub subscriptions: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<Vec<String>>,
    #[serde(default)]
    pub correlated: Vec<String>,
    #[serde(default)]
    pub pending: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluating: Option<Evaluation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expired: Option<u32>,
    #[serde(default)]
    pub meta: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub eval_id: String,
    pub thread_key: String,
    pub trigger_ids: Vec<String>,
    pub policy_version: String,
    pub at: String,
    pub kind: String, // ignore|accept|reopen|stop|unavailable
    pub reason: Vec<Reason>,
    pub strength: Strength,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ticket_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mandate: Option<Mandate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audit: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismissed: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub id: String,
    pub state: String, // pending|running|ended
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<Report>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub state: String, // progress|waiting|ready|blocked|failed|dropped
    pub summary: String,
    pub at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum TicketInput {
    #[serde(rename = "trigger")]
    Trigger {
        id: String,
        trigger_id: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        human: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        read_at: Option<Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_to: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delivered_at: Option<String>,
    },
    #[serde(rename = "policy-changed")]
    PolicyChanged {
        id: String,
        from: String,
        to: String,
        at: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<PolicyEffect>,
    },
    #[serde(rename = "reopen")]
    Reopen { id: String, decision_id: String, at: String },
    #[serde(rename = "resumed")]
    Resumed { id: String, trigger_id: String, at: String },
    #[serde(rename = "text")]
    Text { id: String, text: String, at: String },
    #[serde(rename = "note")]
    Note { id: String, text: String, at: String },
}

impl TicketInput {
    pub fn id(&self) -> &str {
        match self {
            TicketInput::Trigger { id, .. }
            | TicketInput::PolicyChanged { id, .. }
            | TicketInput::Reopen { id, .. }
            | TicketInput::Resumed { id, .. }
            | TicketInput::Text { id, .. }
            | TicketInput::Note { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEffect {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rules: Option<Vec<PolicyEffectRule>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proactive: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester_allowed: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<ActorRef>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEffectRule {
    pub id: String,
    pub name: String,
    pub change: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instruction: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketEnded {
    pub at: String,
    pub by: String, // executor|human
    pub outcome: String, // completed|merged|closed|dropped|stopped
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Attention {
    pub id: String,
    pub question: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger_id: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Follow {
    pub thread: String,
    pub own: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed: Option<FollowClosed>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FollowClosed {
    pub at: String,
    pub outcome: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Execution {
    pub name: String,
    pub directory: String,
    pub assignment: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub decision_id: String,
    pub mandate: Mandate,
    pub channels: Vec<ChannelRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<TicketEnded>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attention: Option<Attention>,
    #[serde(default)]
    pub assignments: Vec<Assignment>,
    #[serde(default)]
    pub inputs: Vec<TicketInput>,
    #[serde(default)]
    pub undelivered: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub follows: Option<Vec<Follow>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answers: Option<Vec<String>>,
    pub created_at: String,
    pub updated_at: String,
    pub origin: TicketOrigin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution: Option<Execution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery: Option<DeliveryError>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TicketOrigin {
    pub trigger_id: String,
    pub trigger_ids: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryError {
    pub error: String,
    pub at: String,
}

pub fn live_ticket(t: &Ticket) -> bool {
    t.ended.is_none()
}
pub fn running_assignment(t: &Ticket) -> Option<&Assignment> {
    t.assignments.iter().find(|a| a.state == "running")
}
pub fn latest_assignment(t: &Ticket) -> Option<&Assignment> {
    t.assignments.last()
}
pub fn latest_report(t: &Ticket) -> Option<&Report> {
    t.assignments.iter().rev().find_map(|a| a.report.as_ref())
}
pub fn read_until(t: &Ticket, progress: bool) -> String {
    let mut best = String::new();
    if let Some(rep) = t
        .assignments
        .iter()
        .rev()
        .find(|a| a.report.is_some() && (progress || a.report.as_ref().unwrap().state != "progress"))
        .and_then(|a| a.report.as_ref())
    {
        if rep.at > best {
            best = rep.at.clone();
        }
    }
    for a in &t.assignments {
        if let Some(idle) = &a.idle_at {
            if *idle > best {
                best = idle.clone();
            }
        }
    }
    best
}
pub fn waits_for_person(t: &Ticket) -> bool {
    let rep = match latest_assignment(t).and_then(|a| a.report.as_ref()) {
        Some(r) => r,
        None => return false,
    };
    if rep.state != "blocked" && rep.state != "failed" {
        return false;
    }
    !t.inputs.iter().any(|inp| match inp {
        TicketInput::Trigger { human, context, at, .. } => {
            human == &Some(true) && context != &Some(true) && *at > rep.at
        }
        _ => false,
    })
}

/// Whether a Trigger reached a ticket (TS `reached`).
pub fn reached(
    ticket: &Ticket,
    trigger: &Trigger,
    thread: &dyn Fn(&str) -> Option<Thread>,
    context: bool,
) -> bool {
    if ticket.origin.trigger_id == trigger.id {
        return true;
    }
    let input = ticket.inputs.iter().any(|i| {
        matches!(i, TicketInput::Trigger { trigger_id, .. } if trigger_id == &trigger.id)
    });
    let via_sub = trigger.threads.iter().any(|r| {
        let cur = thread(&thread_key(r));
        cur.map(|th| th.subscriptions.contains(&ticket.id) && (context || !th.context.clone().unwrap_or_default().contains(&ticket.id)))
            .unwrap_or(false)
    });
    if via_sub {
        return true;
    }
    input
        && (context
            || !trigger.threads.iter().all(|r| {
                thread(&thread_key(r))
                    .map(|th| th.context.clone().unwrap_or_default().contains(&ticket.id))
                    .unwrap_or(false)
            }))
}

/// `followsDone` (TS Tickets.followsDone): None while not done, else outcome.
/// Holds once no own thread is open, or, with no own thread, once every
/// followed thread closed.
pub fn follows_done(ticket: &Ticket) -> Option<String> {
    let follows = ticket.follows.as_ref()?;
    if follows.is_empty() {
        return None;
    }
    let has_own = follows.iter().any(|f| f.own);
    if has_own {
        if follows.iter().any(|f| f.own && f.closed.is_none()) {
            return None;
        }
    } else if follows.iter().any(|f| f.closed.is_none()) {
        return None;
    }
    // Outcome is the close outcome of the (last) closed follow, else completed.
    let out = follows
        .iter()
        .rev()
        .find_map(|f| f.closed.as_ref().map(|c| c.outcome.clone()))
        .unwrap_or_else(|| "completed".to_string());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(id: &str) -> ActorRef {
        ActorRef { source: "github".into(), id: id.into() }
    }
    fn trig(id: &str, human: bool) -> Trigger {
        Trigger {
            id: id.into(),
            source: "github".into(),
            threads: vec![ThreadRef { source: "github".into(), id: "t1".into() }],
            actor: actor("alice"),
            human,
            addressed: Some("mention".into()),
            at: "2026-11-02T00:00:00.000Z".into(),
            text: "hi".into(),
            channel: None,
            url: None,
            meta: serde_json::json!({}),
        }
    }

    #[test]
    fn iso_time_normalizes() {
        assert_eq!(iso_time("2026-11-02T01:02:03Z"), "2026-11-02T01:02:03.000Z");
        assert_eq!(iso_time("not-a-date"), "not-a-date");
    }
    #[test]
    fn thread_key_format() {
        assert_eq!(thread_key(&ThreadRef { source: "github".into(), id: "42".into() }), "github:42");
    }
    #[test]
    fn settled_matrix() {
        let mut d = Decision {
            id: "d".into(),
            eval_id: "e".into(),
            thread_key: "github:t1".into(),
            trigger_ids: vec![],
            policy_version: "v".into(),
            at: now_iso(),
            kind: "accept".into(),
            reason: vec![],
            strength: Strength::Explicit,
            ticket_id: None,
            mandate: None,
            audit: None,
            dismissed: None,
        };
        assert!(settled(&d));
        d.kind = "stop".into();
        assert!(!settled(&d));
        d.kind = "accept".into();
        d.reason = vec![Reason { code: "routed".into(), trigger_id: None, values: None }];
        assert!(!settled(&d));
    }
    #[test]
    fn followup_owners_nonhuman_empty() {
        let t = trig("x", false);
        assert!(followup_owners(&t, &HashMap::new(), &HashMap::new(), &HashMap::new()).is_empty());
    }
    #[test]
    fn follows_done_rules() {
        let mk = |follows: Vec<Follow>| Ticket {
            id: "k".into(),
            title: "t".into(),
            decision_id: "d".into(),
            mandate: Mandate {
                strength: Strength::Explicit,
                policy_version: "v".into(),
                basis: MandateBasis::Addressed { source: None, sender: None, rules: None, addressed: "mention".into() },
                actor: actor("alice"),
                scope: "fix".into(),
            },
            channels: vec![],
            ended: None,
            attention: None,
            assignments: vec![],
            inputs: vec![],
            undelivered: vec![],
            follows: Some(follows),
            answers: None,
            created_at: now_iso(),
            updated_at: now_iso(),
            origin: TicketOrigin { trigger_id: "x".into(), trigger_ids: vec!["x".into()] },
            execution: None,
            delivery: None,
        };
        // own open -> not done
        assert_eq!(
            follows_done(&mk(vec![Follow { thread: "github:pr1".into(), own: true, closed: None }])),
            None
        );
        // own merged -> merged
        assert_eq!(
            follows_done(&mk(vec![Follow {
                thread: "github:pr1".into(),
                own: true,
                closed: Some(FollowClosed { at: now_iso(), outcome: "merged".into() })
            }])),
            Some("merged".into())
        );
    }
}
