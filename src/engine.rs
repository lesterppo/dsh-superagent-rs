//! Engine: single entry point coordinating Intake, Evals, Tickets,
//! Assignments and Replies, ported from `src/core/engine.ts`,
//! `intake.ts`, `evals.ts`, `tickets.ts`, `assignments.ts`, `replies.ts`
//! (simplified where the TS version depends on DSH sessions: the
//! assessor here is a pluggable `Assessor` trait — scripted in tests,
//! model-backed in the agent loop — and replies are recorded in an
//! outbox vector instead of being sent to Feishu/GitHub).

use crate::model::*;
use crate::policy::*;
use crate::store::Store;
use std::collections::HashMap;

pub const REQUEUES: u32 = 2;

#[derive(Debug, Clone)]
pub struct ReceiveResult {
    pub filtered: Option<String>,
    pub eval_started: bool,
    pub fanout: Vec<String>,
    pub queued: Vec<String>,
}

/// What the assessor (orchestrator model, or scripted stand-in) decides.
/// Mirrors TS `AssessmentOutcome` + `Outcome`.
#[derive(Debug, Clone)]
pub struct Outcome {
    pub kind: String, // accept|ignore|stop|unavailable|reopen
    pub reason: String,
    pub title: Option<String>,
    pub scope: Option<String>,
    pub rules: Vec<String>,
    pub ticket_id: Option<String>,
    pub acknowledgement: Option<String>,
    pub name: Option<String>,
    pub eval_id: Option<String>,
}
impl Outcome {
    pub fn ignore(reason: &str) -> Self {
        Outcome { kind: "ignore".into(), reason: reason.into(), title: None, scope: None, rules: vec![], ticket_id: None, acknowledgement: None, name: None, eval_id: None }
    }
    pub fn accept(title: &str, scope: &str) -> Self {
        Outcome { kind: "accept".into(), reason: "explicitRouted".into(), title: Some(title.into()), scope: Some(scope.into()), rules: vec![], ticket_id: None, acknowledgement: None, name: None, eval_id: None }
    }
}

/// Assessor trait: given a thread snapshot + candidate triggers, produce
/// an Outcome. Scripted assessors implement it in tests; the CLI's
/// orchestrator agent implements it via a model provider.
pub trait Assessor {
    fn assess(&mut self, thread: &Thread, triggers: &[Trigger], strength: &Strength, policy: &Settings) -> Outcome;
}

/// A trivial assessor used by the CLI demo when no model is configured:
/// explicit candidates are accepted, inferred ones are ignored (observe).
pub struct RuleAssessor;
impl Assessor for RuleAssessor {
    fn assess(&mut self, _thread: &Thread, triggers: &[Trigger], strength: &Strength, _policy: &Settings) -> Outcome {
        if *strength == Strength::Explicit {
            let t = triggers.first().map(|x| x.text.clone()).unwrap_or_default();
            Outcome::accept(&format!("Work: {}", t.chars().take(60).collect::<String>()), &t)
        } else {
            Outcome::ignore("inferredNotAuto")
        }
    }
}

pub struct Engine {
    pub store: Store,
    pub settings: Settings,
    /// Outbox: replies the engine would send to the source channel.
    /// The TS code sends via channels; here callers drain this.
    pub outbox: Vec<OutboxItem>,
    pub paused: bool,
}

#[derive(Debug, Clone)]
pub struct OutboxItem {
    pub ticket_id: Option<String>,
    pub channel: Option<ChannelRef>,
    pub text: String,
    pub kind: String, // acknowledgement|reply|attention|final
}

impl Engine {
    pub fn new(settings: Settings) -> Self {
        Engine { store: Store::default(), settings, outbox: vec![], paused: false }
    }
    pub fn policy_version(&self) -> String {
        policy_version(&self.settings)
    }

    // ---------- Intake (src/core/intake.ts) ----------
    /// Store a trigger once, fan it out to subscribed tickets, queue it
    /// for Eval on its primary thread unless a ticket's worker decides it.
    pub fn receive(&mut self, trigger: Trigger) -> Result<ReceiveResult, String> {
        if self.store.triggers.contains_key(&trigger.id) {
            // Idempotence: accepting a trigger twice creates one ticket.
            let disp = self.store.dispositions.get(&trigger.id).cloned().unwrap_or(TriggerDisposition { fanout: vec![], at: trigger.at.clone(), ..Default::default() });
            return Ok(ReceiveResult { filtered: None, eval_started: false, fanout: disp.fanout, queued: vec![] });
        }
        let at = iso_time(&trigger.at);
        let mut trigger = trigger;
        trigger.at = at.clone();
        // Primary thread + correlated threads.
        let primary = trigger.threads.first().cloned().ok_or("trigger needs a thread")?;
        for r in &trigger.threads {
            let th = self.store.thread_or_create(r, &at);
            th.last_at = Some(at.clone());
            for other in &trigger.threads {
                let k = thread_key(other);
                if k != th.key && !th.correlated.contains(&k) {
                    th.correlated.push(k);
                }
            }
        }
        // Fan-out to live subscribed tickets of each thread.
        let mut fanout: Vec<String> = vec![];
        let mut context: Vec<String> = vec![];
        for r in &trigger.threads {
            let key = thread_key(r);
            let subs = self.store.threads.get(&key).map(|t| t.subscriptions.clone()).unwrap_or_default();
            let ctx = self.store.threads.get(&key).and_then(|t| t.context.clone()).unwrap_or_default();
            for tid in subs {
                if !fanout.contains(&tid) {
                    fanout.push(tid.clone());
                }
                if ctx.contains(&tid) && !context.contains(&tid) {
                    context.push(tid.clone());
                }
            }
        }
        // Deliver as inputs to live tickets (ended tickets: only a
        // person's message resumes them; handled in tick/resume below).
        let mut delivered_to: Vec<String> = vec![];
        for tid in fanout.clone() {
            let is_context = context.contains(&tid);
            let inp_id = self.store.next_id("input");
            let ticket = match self.store.tickets.get_mut(&tid) {
                Some(t) => t,
                None => continue,
            };
            if let Some(ended) = &ticket.ended {
                if ended.by == "human" {
                    continue; // a ticket a person stopped receives nothing
                }
                if !trigger.human {
                    continue; // bots/CI do not resume ended tickets
                }
                if is_context {
                    continue; // context follows do not resume
                }
                // Person's message resumes an ended ticket under its
                // unchanged mandate, after revalidation.
                if !revalidate(&self.settings, &ticket.mandate) {
                    continue;
                }
                ticket.ended = None;
                ticket.attention = None;
                ticket.inputs.push(TicketInput::Resumed { id: inp_id.clone(), trigger_id: trigger.id.clone(), at: at.clone() });
                ticket.undelivered.push(inp_id);
                ticket.updated_at = at.clone();
                delivered_to.push(tid.clone());
                continue;
            }
            ticket.inputs.push(TicketInput::Trigger {
                id: inp_id.clone(),
                trigger_id: trigger.id.clone(),
                at: at.clone(),
                note: None,
                human: if trigger.human { Some(true) } else { None },
                context: if is_context { Some(true) } else { None },
                read_at: None,
                delivered_to: None,
                delivered_at: None,
            });
            ticket.undelivered.push(inp_id);
            ticket.updated_at = at.clone();
            delivered_to.push(tid.clone());
        }
        // Disposition + queue for eval unless a worker decides it.
        let owners = {
            // Build a temporary disposition to compute owners, then store.
            let mut disp = TriggerDisposition { fanout: fanout.clone(), at: at.clone(), ..Default::default() };
            if !context.is_empty() {
                disp.context = Some(context.clone());
            }
            self.store.dispositions.insert(trigger.id.clone(), disp.clone());
            followup_owners(&trigger, &self.store.tickets, &self.store.threads, &self.store.dispositions)
        };
        let queued = if owners.is_empty() {
            let key = thread_key(&primary);
            let th = self.store.threads.get_mut(&key).unwrap();
            if !th.pending.contains(&trigger.id) {
                th.pending.push(trigger.id.clone());
            }
            vec![trigger.id.clone()]
        } else {
            vec![]
        };
        self.store.triggers.insert(trigger.id.clone(), trigger);
        Ok(ReceiveResult { filtered: None, eval_started: false, fanout: delivered_to, queued })
    }

    /// Bind an open ticket to a thread (`bindThread`); `own` when the
    /// ticket opened it.
    pub fn bind_thread(&mut self, ticket_id: &str, thread: ThreadRef, own: bool) -> Result<Ticket, String> {
        if self.store.tickets.get(ticket_id).map(|t| t.ended.is_some()).unwrap_or(false) {
            return Err("ticketEnded".into());
        }
        if !self.store.tickets.contains_key(ticket_id) {
            return Err("unknownTicket".into());
        }
        let key = thread_key(&thread);
        let at = now_iso();
        let th = self.store.thread_or_create(&thread, &at);
        if !th.subscriptions.contains(&ticket_id.to_string()) {
            th.subscriptions.push(ticket_id.to_string());
        }
        let ticket = self.store.tickets.get_mut(ticket_id).ok_or("unknownTicket")?;
        let follows = ticket.follows.get_or_insert_with(Vec::new);
        if !follows.iter().any(|f| f.thread == key) {
            follows.push(Follow { thread: key, own, closed: None });
        }
        ticket.updated_at = at;
        Ok(ticket.clone())
    }

    /// A thread closed (PR merged/closed) at `at`.
    pub fn thread_closed(&mut self, thread: ThreadRef, outcome: &str, at: &str) -> Result<(), String> {
        let at = iso_time(at);
        let key = thread_key(&thread);
        // Mark follows closed on every ticket following this thread.
        let mut ended_ids: Vec<String> = vec![];
        for (tid, ticket) in self.store.tickets.iter_mut() {
            if let Some(follows) = &mut ticket.follows {
                for f in follows.iter_mut() {
                    if f.thread == key && f.closed.is_none() {
                        f.closed = Some(FollowClosed { at: at.clone(), outcome: outcome.to_string() });
                    }
                }
            }
            if ticket.ended.is_none() {
                if let Some(out) = follows_done(ticket) {
                    // TS `unfinished` (tickets.ts): if the latest
                    // assignment was created before the close, work is
                    // finished unless a taken-but-unread input exists
                    // (approximated here as undelivered trigger inputs);
                    // if it was created after the close (a resume),
                    // work is unfinished until a ready report, no
                    // undelivered inputs, and no inputs after the report.
                    let latest = latest_assignment(ticket);
                    let created_after = latest.map(|a| a.created_at.as_str() > at.as_str()).unwrap_or(false);
                    let unfinished = if !created_after {
                        ticket.inputs.iter().any(|i| matches!(i, TicketInput::Trigger { .. }) && ticket.undelivered.contains(&i.id().to_string()))
                    } else {
                        let rep_state = latest.and_then(|a| a.report.as_ref().map(|r| r.state.clone()));
                        let later = ticket.inputs.iter().any(|i| {
                            let iat = match i {
                                TicketInput::Trigger { at, .. } => at.clone(),
                                TicketInput::PolicyChanged { at, .. } => at.clone(),
                                TicketInput::Reopen { at, .. } => at.clone(),
                                TicketInput::Resumed { at, .. } => at.clone(),
                                TicketInput::Text { at, .. } => at.clone(),
                                TicketInput::Note { at, .. } => at.clone(),
                            };
                            let rep_at = latest.and_then(|a| a.report.as_ref().map(|r| r.at.clone())).unwrap_or_default();
                            !matches!(i, TicketInput::PolicyChanged { .. }) && iat > rep_at
                        });
                        rep_state.as_deref() != Some("ready") || !ticket.undelivered.is_empty() || later
                    };
                    if !unfinished {
                        ended_ids.push(tid.clone());
                        let _ = out;
                    }
                }
            }
        }
        for tid in ended_ids {
            let out = {
                let t = self.store.tickets.get(&tid).unwrap();
                follows_done(t).unwrap_or_else(|| "completed".into())
            };
            self.end_ticket(&tid, "executor", &out, &at)?;
        }
        Ok(())
    }

    // ---------- Evals ----------
    /// Start evals for threads with queued triggers (the "pass").
    /// Returns decisions made (via the assessor) in this pass.
    pub fn tick<A: Assessor>(&mut self, assessor: &mut A) -> Vec<Decision> {
        let mut made: Vec<Decision> = vec![];
        if self.paused || self.settings.paused {
            return made;
        }
        // Expire overdue evals first.
        let keys: Vec<String> = self.store.threads.keys().cloned().collect();
        for key in keys {
            self.expire_if_due(&key);
        }
        // Start evals for threads with pending triggers and no eval.
        let keys: Vec<String> = self.store.threads.keys().cloned().collect();
        for key in keys {
            let (has_pending, has_eval) = match self.store.threads.get(&key) {
                Some(th) => (!th.pending.is_empty(), th.evaluating.is_some()),
                None => (false, false),
            };
            if has_pending && !has_eval {
                if let Some(d) = self.start_eval(&key, assessor) {
                    made.push(d);
                }
            }
        }
        made
    }

    fn expire_if_due(&mut self, key: &str) {
        let now = now_iso();
        let (deadline, delivered, paused_at) = match self.store.threads.get(key).and_then(|t| t.evaluating.as_ref()) {
            Some(ev) => (ev.deadline.clone(), ev.delivered.clone(), ev.paused_at.clone()),
            None => return,
        };
        if self.paused || self.settings.paused {
            if paused_at.is_none() {
                if let Some(th) = self.store.threads.get_mut(key) {
                    if let Some(ev) = &mut th.evaluating {
                        ev.paused_at = Some(now);
                    }
                }
            }
            return;
        }
        if let Some(p) = paused_at {
            // Resume: shift deadline forward by pause length.
            if let (Ok(d), Ok(n), Ok(pp)) = (
                chrono::DateTime::parse_from_rfc3339(&deadline),
                chrono::DateTime::parse_from_rfc3339(&now),
                chrono::DateTime::parse_from_rfc3339(&p),
            ) {
                let shifted = d + (n - pp);
                if let Some(th) = self.store.threads.get_mut(key) {
                    if let Some(ev) = &mut th.evaluating {
                        ev.deadline = shifted.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
                        ev.paused_at = None;
                    }
                }
            }
            return;
        }
        if delivered.is_some() && deadline <= now {
            self.expire_eval(key);
        }
    }

    fn expire_eval(&mut self, key: &str) {
        let expired = {
            let th = match self.store.threads.get_mut(key) {
                Some(t) => t,
                None => return,
            };
            let ev = match th.evaluating.take() {
                Some(e) => e,
                None => return,
            };
            let count = th.expired.unwrap_or(0) + 1;
            if count > REQUEUES {
                th.expired = None;
                // finish as unavailable
                let at = now_iso();
                let dec = Decision {
                    id: self.store.next_id("decision"),
                    eval_id: ev.id.clone(),
                    thread_key: key.to_string(),
                    trigger_ids: ev.trigger_ids.clone(),
                    policy_version: self.policy_version(),
                    at,
                    kind: "unavailable".into(),
                    reason: vec![Reason { code: "timeout".into(), trigger_id: None, values: None }],
                    strength: ev.strength.clone(),
                    ticket_id: None,
                    mandate: None,
                    audit: None,
                    dismissed: None,
                };
                self.store.decisions.insert(dec.id.clone(), dec);
                return;
            }
            th.expired = Some(count);
            let mut pending = ev.trigger_ids.clone();
            pending.extend(th.pending.iter().filter(|id| !ev.trigger_ids.contains(id)).cloned());
            th.pending = pending;
            return;
        };
        let _ = expired;
    }

    /// Start one eval of a thread's queued triggers, judged by the gate,
    /// then by the assessor. Returns the Decision if one is recorded
    /// synchronously (ignore / accept / stop); evals that need a model
    /// are decided through `decide` by the caller.
    fn start_eval<A: Assessor>(&mut self, key: &str, assessor: &mut A) -> Option<Decision> {
        // Remove follow-ups a ticket still decides (they leave the queue).
        let followups: Vec<String> = {
            let th = match self.store.threads.get(key) {
                Some(t) => t,
                None => return None,
            };
            th.pending
                .iter()
                .filter(|id| {
                    self.store.triggers.get(*id).map(|tr| {
                        !followup_owners(tr, &self.store.tickets, &self.store.threads, &self.store.dispositions).is_empty()
                    }).unwrap_or(false)
                })
                .cloned()
                .collect()
        };
        if !followups.is_empty() {
            if let Some(th) = self.store.threads.get_mut(key) {
                th.pending.retain(|id| !followups.contains(id));
            }
            if self.store.threads.get(key).map(|t| t.pending.is_empty()).unwrap_or(true) {
                return None;
            }
        }
        // Triggers that already originated a ticket cannot spawn another.
        let accepted_ids: std::collections::HashMap<String, String> = self
            .store
            .decisions
            .values()
            .filter(|d| d.kind == "accept")
            .flat_map(|d| d.trigger_ids.iter().map(|tid| (tid.clone(), d.ticket_id.clone().unwrap_or_default())).collect::<Vec<_>>())
            .collect();
        let pending: Vec<String> = self.store.threads.get(key).map(|t| t.pending.clone()).unwrap_or_default();
        let started: Vec<String> = pending.iter().filter(|id| accepted_ids.contains_key(*id)).cloned().collect();
        if !started.is_empty() && started.len() == pending.len() {
            let at = now_iso();
            let reasons: Vec<Reason> = started
                .iter()
                .map(|id| Reason { code: "startedTicket".into(), trigger_id: Some(id.clone()), values: Some(HashMap::from([("ticket".into(), accepted_ids[id].clone())])) })
                .collect();
            return Some(self.finish_ignore(key, pending, reasons, &at));
        }
        // Gate each trigger; strongest admissible wins (TS logic).
        let triggers: Vec<Trigger> = pending
            .iter()
            .filter_map(|id| self.store.triggers.get(id).cloned())
            .collect();
        let tickets_on_thread: Vec<Ticket> = self
            .store
            .tickets
            .values()
            .filter(|t| {
                let th = self.store.threads.get(key);
                th.map(|th| th.subscriptions.contains(&t.id)).unwrap_or(false)
            })
            .cloned()
            .collect();
        let mut strongest: Option<(Trigger, Strength, MandateBasis)> = None;
        let mut reasons: Vec<Reason> = vec![];
        for tr in &triggers {
            let th = self.store.threads.get(key).unwrap().clone();
            let g = gate(&self.settings, tr, &th, &tickets_on_thread);
            if !g.admissible {
                reasons.push(Reason { code: g.reason.unwrap_or_else(|| "noAdmissible".into()), trigger_id: Some(tr.id.clone()), values: None });
                continue;
            }
            let (strength, basis) = (g.strength.unwrap(), g.basis.unwrap());
            let replace = match &strongest {
                None => true,
                Some((old_tr, old_s, _)) => {
                    let stronger = matches!((old_s, &strength), (Strength::Inferred, Strength::Explicit));
                    stronger || (old_s == &strength && tr.at >= old_tr.at)
                }
            };
            if replace {
                strongest = Some((tr.clone(), strength, basis));
            }
        }
        let (primary, strength, basis) = match strongest {
            Some((t, s, b)) => (t, s, b),
            None => {
                let at = now_iso();
                if pending.is_empty() {
                    return None;
                }
                let r = if reasons.is_empty() { vec![Reason { code: "noAdmissible".into(), trigger_id: None, values: None }] } else { reasons };
                return Some(self.finish_ignore(key, pending, r, &at));
            }
        };
        // Open the eval, then ask the assessor and decide synchronously.
        let at = now_iso();
        let eval_id = self.store.next_id("eval");
        let deadline = {
            let dt = chrono::Utc::now() + chrono::Duration::seconds(self.settings.eval_timeout_seconds as i64);
            dt.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
        };
        let eval = Evaluation {
            id: eval_id.clone(),
            thread_key: key.to_string(),
            trigger_ids: pending.clone(),
            primary: Some(primary.id.clone()),
            strength: strength.clone(),
            actor: primary.actor.clone(),
            basis: basis.clone(),
            policy_version: self.policy_version(),
            started_at: at.clone(),
            delivered: Some(at.clone()),
            deadline,
            paused_at: None,
        };
        if let Some(th) = self.store.threads.get_mut(key) {
            th.evaluating = Some(eval.clone());
            th.pending = vec![];
        }
        for tid in &pending {
            if let Some(d) = self.store.dispositions.get_mut(tid) {
                d.eval_id = Some(eval_id.clone());
            }
        }
        let thread_snapshot = self.store.threads.get(key).unwrap().clone();
        let outcome = assessor.assess(&thread_snapshot, &triggers, &strength, &self.settings);
        // Inferred accept outside auto -> would_accept ignore (TS assessmentOutcome).
        let outcome = if outcome.kind == "accept" && strength == Strength::Inferred && !admits_inferred(&self.settings, &outcome.rules) {
            Outcome::ignore("observed")
        } else {
            outcome
        };
        Some(self.finish_eval(key, eval, outcome, &primary))
    }

    fn finish_ignore(&mut self, key: &str, trigger_ids: Vec<String>, reasons: Vec<Reason>, at: &str) -> Decision {
        if let Some(th) = self.store.threads.get_mut(key) {
            th.pending.retain(|id| !trigger_ids.contains(id));
            th.expired = None;
        }
        let dec = Decision {
            id: self.store.next_id("decision"),
            eval_id: self.store.next_id("eval"),
            thread_key: key.to_string(),
            trigger_ids: trigger_ids.clone(),
            policy_version: self.policy_version(),
            at: at.to_string(),
            kind: "ignore".into(),
            reason: reasons,
            strength: Strength::Inferred,
            ticket_id: None,
            mandate: None,
            audit: None,
            dismissed: None,
        };
        self.store.decisions.insert(dec.id.clone(), dec.clone());
        for tid in &trigger_ids {
            if let Some(d) = self.store.dispositions.get_mut(tid) {
                d.decision_id = Some(dec.id.clone());
            }
        }
        dec
    }

    /// Finish an eval with an outcome (used by start_eval and by direct decide).
    fn finish_eval(&mut self, key: &str, eval: Evaluation, outcome: Outcome, primary: &Trigger) -> Decision {
        let at = now_iso();
        if let Some(th) = self.store.threads.get_mut(key) {
            th.evaluating = None;
            th.expired = None;
        }
        let mut dec = Decision {
            id: self.store.next_id("decision"),
            eval_id: eval.id.clone(),
            thread_key: key.to_string(),
            trigger_ids: eval.trigger_ids.clone(),
            policy_version: self.policy_version(),
            at: at.clone(),
            kind: outcome.kind.clone(),
            reason: vec![Reason { code: outcome.reason.clone(), trigger_id: None, values: None }],
            strength: eval.strength.clone(),
            ticket_id: None,
            mandate: None,
            audit: None,
            dismissed: None,
        };
        if outcome.kind == "accept" {
            let title = outcome.title.clone().unwrap_or_else(|| primary.text.clone());
            let scope = outcome.scope.clone().unwrap_or_else(|| title.clone());
            let mandate = Mandate {
                strength: eval.strength.clone(),
                policy_version: eval.policy_version.clone(),
                basis: eval.basis.clone(),
                actor: eval.actor.clone(),
                scope: scope.clone(),
            };
            let ticket_id = self.store.next_id("ticket");
            let channels: Vec<ChannelRef> = primary.channel.clone().into_iter().collect();
            let ticket = Ticket {
                id: ticket_id.clone(),
                title,
                decision_id: dec.id.clone(),
                mandate: mandate.clone(),
                channels,
                ended: None,
                attention: None,
                assignments: vec![],
                inputs: vec![],
                undelivered: vec![],
                follows: None,
                answers: None,
                created_at: at.clone(),
                updated_at: at.clone(),
                origin: TicketOrigin { trigger_id: primary.id.clone(), trigger_ids: eval.trigger_ids.clone() },
                execution: None,
                delivery: None,
            };
            self.store.tickets.insert(ticket_id.clone(), ticket);
            // Subscribe the ticket to the primary thread (its origin thread).
            if let Some(th) = self.store.threads.get_mut(key) {
                if !th.subscriptions.contains(&ticket_id) {
                    th.subscriptions.push(ticket_id.clone());
                }
            }
            dec.ticket_id = Some(ticket_id.clone());
            dec.mandate = Some(mandate);
            if let Some(ack) = &outcome.acknowledgement {
                self.outbox.push(OutboxItem {
                    ticket_id: Some(ticket_id),
                    channel: primary.channel.clone(),
                    text: ack.clone(),
                    kind: "acknowledgement".into(),
                });
            }
            // Start a pending assignment for the new ticket.
            let _ = self.start_assignment(&dec.ticket_id.clone().unwrap(), &at);
        } else if outcome.kind == "stop" {
            if let Some(tid) = &outcome.ticket_id {
                let _ = self.end_ticket(tid, "human", "stopped", &at);
                dec.ticket_id = Some(tid.clone());
            }
        }
        self.store.decisions.insert(dec.id.clone(), dec.clone());
        for tid in &eval.trigger_ids {
            if let Some(d) = self.store.dispositions.get_mut(tid) {
                d.decision_id = Some(dec.id.clone());
            }
        }
        dec
    }

    /// Direct decide of a trigger (orchestrator_route path). Simplified
    /// vs TS: if an eval holds it, finish that eval; else decide it alone.
    pub fn decide(&mut self, trigger_id: &str, outcome: Outcome) -> Result<Decision, String> {
        let trigger = self.store.triggers.get(trigger_id).cloned().ok_or("unknownTrigger")?;
        // Already accepted -> return the earlier decision (no duplicate ticket).
        if outcome.kind == "accept" {
            for d in self.store.decisions.values() {
                if d.kind == "accept" && d.trigger_ids.contains(&trigger_id.to_string()) {
                    return Ok(d.clone());
                }
            }
        }
        let primary_key = trigger.threads.first().map(|r| thread_key(r)).ok_or("noThread")?;
        if let Some(ev) = self.store.threads.get(&primary_key).and_then(|t| t.evaluating.clone()) {
            if let Some(want) = &outcome.eval_id {
                if *want != ev.id {
                    return Err("evalFinished".into());
                }
            }
            // Worker still decides a follow-up: only stop may override.
            if outcome.kind != "stop" {
                let owners = followup_owners(&trigger, &self.store.tickets, &self.store.threads, &self.store.dispositions);
                if !owners.is_empty() {
                    return Err("workerDecides".into());
                }
            }
            return Ok(self.finish_eval(&primary_key, ev, outcome, &trigger));
        }
        // No eval holds it: direct eval.
        let eval = Evaluation {
            id: self.store.next_id("eval"),
            thread_key: primary_key.clone(),
            trigger_ids: vec![trigger_id.to_string()],
            primary: Some(trigger_id.to_string()),
            strength: Strength::Explicit,
            actor: trigger.actor.clone(),
            basis: MandateBasis::Addressed {
                source: Some(trigger.source.clone()),
                sender: None,
                rules: None,
                addressed: trigger.addressed.clone().unwrap_or_else(|| "mention".into()),
            },
            policy_version: self.policy_version(),
            started_at: now_iso(),
            delivered: None,
            deadline: now_iso(),
            paused_at: None,
        };
        Ok(self.finish_eval(&primary_key, eval, outcome, &trigger))
    }

    // ---------- Tickets / Assignments ----------
    pub fn start_assignment(&mut self, ticket_id: &str, at: &str) -> Result<String, String> {
        // Enforce max_workers across running assignments.
        let running = self.store.tickets.values().flat_map(|t| t.assignments.iter()).filter(|a| a.state == "running").count();
        if self.store.tickets.get(ticket_id).map(|t| t.ended.is_some()).unwrap_or(false) {
            return Err("ticketEnded".into());
        }
        let id = self.store.next_id("assignment");
        let ticket = self.store.tickets.get_mut(ticket_id).ok_or("unknownTicket")?;
        if running >= self.settings.max_workers {
            // Still create a pending assignment; the worker pool starts it.
            ticket.assignments.push(Assignment {
                id: id.clone(),
                state: "pending".into(),
                created_at: at.to_string(),
                started_at: None,
                ended_at: None,
                ended_reason: None,
                report: None,
                delivered_at: None,
                idle_at: None,
            });
            return Ok(id);
        }
        ticket.assignments.push(Assignment {
            id: id.clone(),
            state: "running".into(),
            created_at: at.to_string(),
            started_at: Some(at.to_string()),
            ended_at: None,
            ended_reason: None,
            report: None,
            delivered_at: None,
            idle_at: None,
        });
        // Deliver undelivered inputs to it.
        let undelivered = ticket.undelivered.clone();
        for inp_id in &undelivered {
            if let Some(inp) = ticket.inputs.iter_mut().find(|i| i.id() == inp_id) {
                if let TicketInput::Trigger { delivered_to, .. } = inp {
                    *delivered_to = Some(id.clone());
                }
            }
        }
        ticket.undelivered = vec![];
        ticket.updated_at = at.to_string();
        Ok(id)
    }

    /// A worker report (TS Assignments.report).
    pub fn report(&mut self, ticket_id: &str, assignment_id: &str, state: &str, summary: &str, question: Option<String>, answers: Option<String>) -> Result<Ticket, String> {
        let at = now_iso();
        let att_id = if question.is_some() { Some(self.store.next_id("attention")) } else { None };
        let ticket = self.store.tickets.get_mut(ticket_id).ok_or("unknownTicket")?;
        if ticket.ended.is_some() {
            return Err("ticketEnded".into());
        }
        let assignment = ticket
            .assignments
            .iter_mut()
            .find(|a| a.id == assignment_id && a.state == "running")
            .ok_or("assignmentNotRunning")?;
        let previous_question = ticket.attention.clone();
        assignment.report = Some(Report { state: state.to_string(), summary: summary.to_string(), at: at.clone() });
        if state == "ready" || state == "dropped" || state == "blocked" || state == "failed" {
            // Terminal-ish reports end the assignment; ready/dropped may
            // end the ticket (see below, after releasing borrow).
        }
        if let Some(q) = question {
            ticket.attention = Some(Attention { id: att_id.unwrap(), question: q, trigger_id: None });
        } else if state == "ready" {
            ticket.attention = None;
        }
        let _ = previous_question;
        if let Some(ans) = answers {
            let cur = ticket.answers.get_or_insert_with(Vec::new);
            if !cur.contains(&ans) {
                cur.push(ans);
            }
        }
        ticket.updated_at = at.clone();
        let terminal = matches!(state, "ready" | "dropped" | "blocked" | "failed");
        if terminal {
            if let Some(a) = ticket.assignments.iter_mut().find(|x| x.id == assignment_id) {
                a.state = "ended".into();
                a.ended_at = Some(at.clone());
                a.ended_reason = Some(state.to_string());
            }
        }
        // Ready ends the ticket once followsDone holds; dropped ends it
        // with follows outcome or dropped.
        let outcome: Option<String> = if state == "ready" {
            match &ticket.follows {
                None => Some("completed".into()),
                Some(_) => follows_done(ticket),
            }
        } else if state == "dropped" {
            Some(follows_done(ticket).unwrap_or_else(|| "dropped".into()))
        } else {
            None
        };
        if let Some(out) = outcome {
            // end_ticket needs &mut self; drop borrow by cloning id.
            let tid = ticket_id.to_string();
            drop(ticket);
            self.end_ticket(&tid, "executor", &out, &at)?;
        }
        Ok(self.store.tickets.get(ticket_id).unwrap().clone())
    }

    pub fn end_ticket(&mut self, ticket_id: &str, by: &str, outcome: &str, at: &str) -> Result<Ticket, String> {
        let ticket = self.store.tickets.get_mut(ticket_id).ok_or("unknownTicket")?;
        if ticket.ended.is_some() {
            return Ok(ticket.clone());
        }
        ticket.ended = Some(TicketEnded { at: at.to_string(), by: by.to_string(), outcome: outcome.to_string(), reason: None });
        ticket.attention = None;
        for a in ticket.assignments.iter_mut() {
            if a.state == "running" {
                a.state = "ended".into();
                a.ended_at = Some(at.to_string());
                a.ended_reason = Some("ticketEnded".into());
            }
        }
        ticket.updated_at = at.to_string();
        Ok(ticket.clone())
    }

    /// Worker hands a trigger back (beyond the ticket); queued for fresh
    /// eval once no other ticket still decides it. Once per ticket.
    pub fn handback(&mut self, ticket_id: &str, trigger_id: &str, reason: &str) -> Result<bool, String> {
        let at = now_iso();
        let ticket = self.store.tickets.get(ticket_id).ok_or("unknownTicket")?.clone();
        if !ticket.inputs.iter().any(|i| matches!(i, TicketInput::Trigger { trigger_id: t, .. } if t == trigger_id)) {
            return Err("notReached".into());
        }
        let disp = self.store.dispositions.get_mut(trigger_id).ok_or("unknownTrigger")?;
        if disp.handbacks.as_ref().map(|h| h.iter().any(|x| x.ticket_id == ticket_id)).unwrap_or(false) {
            return Err("handedBack".into());
        }
        let hb = disp.handbacks.get_or_insert_with(Vec::new);
        hb.push(Handback { ticket_id: ticket_id.to_string(), reason: reason.to_string(), at: at.clone() });
        // If another ticket still decides it, do not queue.
        let trigger = self.store.triggers.get(trigger_id).cloned().ok_or("unknownTrigger")?;
        let owners = followup_owners(&trigger, &self.store.tickets, &self.store.threads, &self.store.dispositions);
        if !owners.is_empty() {
            return Ok(false);
        }
        // Queue for fresh eval on its primary thread.
        let key = thread_key(&trigger.threads[0]);
        if let Some(th) = self.store.threads.get_mut(&key) {
            if !th.pending.contains(&trigger_id.to_string()) {
                th.pending.push(trigger_id.to_string());
            }
        }
        if let Some(d) = self.store.dispositions.get_mut(trigger_id) {
            d.requeued = Some(true);
            // A hand-back requeue is NOT a person requeue: followupOwners
            // must still see it as owned-free; TS distinguishes via the
            // handbacks list, and requeued is set only by reevaluate.
            // Undo the flag: handback queues without setting requeued.
            d.requeued = None;
        }
        Ok(true)
    }

    /// Worker acknowledges / takes up a trigger within the ticket.
    pub fn ack(&mut self, ticket_id: &str, trigger_id: &str) -> Result<(), String> {
        let disp = self.store.dispositions.get_mut(trigger_id).ok_or("unknownTrigger")?;
        let acked = disp.acked.get_or_insert_with(Vec::new);
        if !acked.contains(&ticket_id.to_string()) {
            acked.push(ticket_id.to_string());
        }
        Ok(())
    }

    /// Retry a blocked/failed ticket: end the stalled assignment, start a new one.
    pub fn retry_ticket(&mut self, ticket_id: &str) -> Result<Ticket, String> {
        let at = now_iso();
        let ticket = self.store.tickets.get_mut(ticket_id).ok_or("unknownTicket")?;
        if ticket.ended.is_some() {
            return Err("ticketEnded".into());
        }
        for a in ticket.assignments.iter_mut() {
            if a.state == "running" {
                a.state = "ended".into();
                a.ended_at = Some(at.clone());
                a.ended_reason = Some("retry".into());
            }
        }
        drop(ticket);
        let _ = self.start_assignment(ticket_id, &at)?;
        Ok(self.store.tickets.get(ticket_id).unwrap().clone())
    }

    /// Requeue a trigger for fresh eval (person's requeue; false while
    /// an eval holds it).
    pub fn reevaluate(&mut self, trigger_id: &str) -> Result<bool, String> {
        let trigger = self.store.triggers.get(trigger_id).cloned().ok_or("unknownTrigger")?;
        let key = thread_key(&trigger.threads[0]);
        if self.store.threads.get(&key).and_then(|t| t.evaluating.as_ref()).map(|e| e.trigger_ids.contains(&trigger_id.to_string())).unwrap_or(false) {
            return Ok(false);
        }
        if let Some(th) = self.store.threads.get_mut(&key) {
            if !th.pending.contains(&trigger_id.to_string()) {
                th.pending.push(trigger_id.to_string());
            }
        }
        if let Some(d) = self.store.dispositions.get_mut(trigger_id) {
            d.requeued = Some(true);
        }
        Ok(true)
    }

    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        self.settings.paused = paused;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::*;

    pub fn demo_settings() -> Settings {
        let mut s = Settings::default();
        s.eval_mode = AcceptMode::Auto;
        let mut src = SourcePolicy::default();
        src.enabled = true;
        src.allowed.push(Allowed { id: "alice".into(), name: None });
        s.sources.insert("github".into(), src);
        s
    }
    pub fn demo_trigger(id: &str, thread: &str, actor: &str, addressed: Option<&str>) -> Trigger {
        Trigger {
            id: id.into(),
            source: "github".into(),
            threads: vec![ThreadRef { source: "github".into(), id: thread.into() }],
            actor: ActorRef { source: "github".into(), id: actor.into() },
            human: true,
            addressed: addressed.map(|x| x.to_string()),
            at: now_iso(),
            text: "Fix the typo in README".into(),
            channel: None,
            url: None,
            meta: serde_json::json!({}),
        }
    }

    #[test]
    fn accept_creates_exactly_one_ticket() {
        let mut e = Engine::new(demo_settings());
        let mut assessor = RuleAssessor;
        e.receive(demo_trigger("trig-1", "issue-1", "alice", Some("mention"))).unwrap();
        let decisions = e.tick(&mut assessor);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].kind, "accept");
        assert_eq!(e.store.tickets.len(), 1);
        // Receiving the same trigger again must not create a second ticket.
        e.receive(demo_trigger("trig-1", "issue-1", "alice", Some("mention"))).unwrap();
        let decisions2 = e.tick(&mut assessor);
        assert!(decisions2.is_empty());
        assert_eq!(e.store.tickets.len(), 1);
        assert!(e.store.check_invariants().is_empty());
    }
    #[test]
    fn unlisted_trigger_is_ignored() {
        let mut e = Engine::new(demo_settings());
        let mut assessor = RuleAssessor;
        e.receive(demo_trigger("trig-9", "issue-9", "mallory", None)).unwrap();
        let decisions = e.tick(&mut assessor);
        assert_eq!(decisions.len(), 1);
        assert_eq!(decisions[0].kind, "ignore");
        assert!(e.store.tickets.is_empty());
    }
    #[test]
    fn ready_report_completes_ticket() {
        let mut e = Engine::new(demo_settings());
        let mut assessor = RuleAssessor;
        e.receive(demo_trigger("trig-1", "issue-1", "alice", Some("mention"))).unwrap();
        e.tick(&mut assessor);
        let tid = e.store.tickets.keys().next().unwrap().clone();
        let aid = e.store.tickets[&tid].assignments.iter().find(|a| a.state == "running").unwrap().id.clone();
        let t = e.report(&tid, &aid, "ready", "Fixed typo, tests pass", None, None).unwrap();
        assert_eq!(t.ended.as_ref().unwrap().outcome, "completed");
    }
    #[test]
    fn expiry_requeues_then_unavailable() {
        let mut e = Engine::new(demo_settings());
        // Manually craft an overdue eval with delivered set.
        e.receive(demo_trigger("trig-1", "issue-1", "alice", Some("mention"))).unwrap();
        // Drive eval open but intercept: create eval directly.
        let key = "github:issue-1".to_string();
        let th = e.store.threads.get_mut(&key).unwrap();
        th.pending = vec![];
        th.evaluating = Some(Evaluation {
            id: "eval-1".into(),
            thread_key: key.clone(),
            trigger_ids: vec!["trig-1".into()],
            primary: Some("trig-1".into()),
            strength: Strength::Explicit,
            actor: ActorRef { source: "github".into(), id: "alice".into() },
            basis: MandateBasis::Addressed { source: None, sender: None, rules: None, addressed: "mention".into() },
            policy_version: "v".into(),
            started_at: "2026-01-01T00:00:00.000Z".into(),
            delivered: Some("2026-01-01T00:00:00.000Z".into()),
            deadline: "2026-01-01T00:01:00.000Z".into(),
            paused_at: None,
        });
        for _ in 0..3 {
            e.expire_if_due(&key);
            // After each expiry except the last, pending is restored;
            // re-open the eval to simulate the next pass timing out again.
            if e.store.threads[&key].evaluating.is_none() && e.store.threads[&key].pending.contains(&"trig-1".to_string()) {
                let th = e.store.threads.get_mut(&key).unwrap();
                th.pending = vec![];
                th.evaluating = Some(Evaluation {
                    id: "eval-1".into(),
                    thread_key: key.clone(),
                    trigger_ids: vec!["trig-1".into()],
                    primary: Some("trig-1".into()),
                    strength: Strength::Explicit,
                    actor: ActorRef { source: "github".into(), id: "alice".into() },
                    basis: MandateBasis::Addressed { source: None, sender: None, rules: None, addressed: "mention".into() },
                    policy_version: "v".into(),
                    started_at: "2026-01-01T00:00:00.000Z".into(),
                    delivered: Some("2026-01-01T00:00:00.000Z".into()),
                    deadline: "2026-01-01T00:01:00.000Z".into(),
                    paused_at: None,
                });
            }
        }
        // 3rd expiry (> REQUEUES=2) records unavailable.
        assert!(e.store.decisions.values().any(|d| d.kind == "unavailable"));
    }
    #[test]
    fn pause_shifts_deadline() {
        let mut e = Engine::new(demo_settings());
        e.receive(demo_trigger("trig-1", "issue-1", "alice", Some("mention"))).unwrap();
        let key = "github:issue-1".to_string();
        // Open an eval via tick with an assessor that we then replace:
        // instead craft directly, then pause/resume through expire_if_due.
        let th = e.store.threads.get_mut(&key).unwrap();
        th.pending = vec![];
        th.evaluating = Some(Evaluation {
            id: "eval-1".into(),
            thread_key: key.clone(),
            trigger_ids: vec!["trig-1".into()],
            primary: Some("trig-1".into()),
            strength: Strength::Explicit,
            actor: ActorRef { source: "github".into(), id: "alice".into() },
            basis: MandateBasis::Addressed { source: None, sender: None, rules: None, addressed: "mention".into() },
            policy_version: "v".into(),
            started_at: now_iso(),
            delivered: Some(now_iso()),
            deadline: "2999-01-01T00:00:00.000Z".into(),
            paused_at: None,
        });
        e.set_paused(true);
        e.expire_if_due(&key);
        assert!(e.store.threads[&key].evaluating.as_ref().unwrap().paused_at.is_some());
        e.set_paused(false);
        e.expire_if_due(&key);
        assert!(e.store.threads[&key].evaluating.as_ref().unwrap().paused_at.is_none());
    }
}
