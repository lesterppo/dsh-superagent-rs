//! Policy gate, ported from `src/policy.ts` + `src/admission.ts`.
//! Defaults: listedUnaddressed=inferred, unlistedAddressed=ignore,
//! unlistedUnaddressed=ignore. A listed sender who addresses the agent
//! is always explicit (no table cell). `observed` in trigger meta means
//! the Source observes all messages in that scope (unaddressed messages
//! only use the table then).

use crate::model::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub const POLICY_REVISION: &str = "mandate-v3";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allowed {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdmissionRow {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub listed_unaddressed: Option<String>, // inferred|ignore
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlisted_addressed: Option<String>, // explicit|inferred|ignore
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unlisted_unaddressed: Option<String>, // inferred|ignore
}
impl AdmissionRow {
    pub fn get(&self, cell: &AdmissionCell) -> Option<&str> {
        match cell {
            AdmissionCell::ListedUnaddressed => self.listed_unaddressed.as_deref(),
            AdmissionCell::UnlistedAddressed => self.unlisted_addressed.as_deref(),
            AdmissionCell::UnlistedUnaddressed => self.unlisted_unaddressed.as_deref(),
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SourcePolicy {
    #[serde(default)]
    pub allowed: Vec<Allowed>,
    #[serde(default)]
    pub bots: Vec<Allowed>,
    #[serde(default)]
    pub admission: HashMap<String, AdmissionRow>,
    #[serde(default)]
    pub enabled: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub instruction: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    pub paused: bool,
    pub eval_mode: AcceptMode,
    pub eval_timeout_seconds: u64,
    pub max_workers: usize,
    #[serde(default)]
    pub blocked_commands: Vec<String>,
    #[serde(default)]
    pub rules: Vec<Rule>,
    #[serde(default)]
    pub sources: HashMap<String, SourcePolicy>,
}
impl Default for Settings {
    fn default() -> Self {
        Settings {
            paused: false,
            eval_mode: AcceptMode::Observe,
            eval_timeout_seconds: 120,
            max_workers: 3,
            blocked_commands: vec![],
            rules: vec![
                Rule {
                    id: "copy".into(),
                    name: "Copy fixes".into(),
                    enabled: true,
                    instruction: "Explicit requests to correct a typo, wording, punctuation or existing translation.".into(),
                },
                Rule {
                    id: "visual".into(),
                    name: "Visual fixes".into(),
                    enabled: true,
                    instruction: "Explicit implementation requests for local spacing, alignment, overflow or styling, with a specified target or an objectively established existing standard.".into(),
                },
            ],
            sources: HashMap::new(),
        }
    }
}

pub fn same_name(a: &str, b: &str) -> bool {
    a.to_lowercase() == b.to_lowercase()
}
pub fn allowed_sender<'a>(settings: &'a Settings, actor: &ActorRef) -> Option<&'a Allowed> {
    settings
        .sources
        .get(&actor.source)
        .and_then(|s| s.allowed.iter().find(|e| same_name(&e.id, &actor.id)))
}
pub fn listed_bot<'a>(settings: &'a Settings, actor: &ActorRef) -> Option<&'a Allowed> {
    settings
        .sources
        .get(&actor.source)
        .and_then(|s| s.bots.iter().find(|e| same_name(&e.id, &actor.id)))
}
pub fn policy_version(settings: &Settings) -> String {
    let payload = serde_json::json!({
        "rules": settings.rules,
        "mode": settings.eval_mode,
        "sources": settings.sources,
    });
    let mut h = Sha256::new();
    h.update(POLICY_REVISION.as_bytes());
    h.update(serde_json::to_string(&payload).unwrap_or_default().as_bytes());
    format!("{:x}", h.finalize())[..32].to_string()
}

fn admission_default(cell: &AdmissionCell) -> &'static str {
    match cell {
        AdmissionCell::ListedUnaddressed => "inferred",
        AdmissionCell::UnlistedAddressed => "ignore",
        AdmissionCell::UnlistedUnaddressed => "ignore",
    }
}
fn admission_of(settings: &Settings, source: &str, row: Option<&str>, cell: &AdmissionCell) -> String {
    if let Some(r) = row {
        if let Some(v) = settings
            .sources
            .get(source)
            .and_then(|s| s.admission.get(r))
            .and_then(|rowobj| rowobj.get(cell))
        {
            return v.to_string();
        }
    }
    admission_default(cell).to_string()
}
fn admission_cell(allowed: bool, addressed: bool) -> Option<AdmissionCell> {
    if allowed {
        if addressed {
            None
        } else {
            Some(AdmissionCell::ListedUnaddressed)
        }
    } else if addressed {
        Some(AdmissionCell::UnlistedAddressed)
    } else {
        Some(AdmissionCell::UnlistedUnaddressed)
    }
}
/// Is the source listened to, and does it observe all messages in scope?
/// Rust simplification of the TS `SourceScope.scope()`: a configured
/// source is listened; `observed` comes from trigger meta `observed=true`
/// (tests and CLI set it explicitly), default false.
pub fn scope_of(trigger: &Trigger, settings: &Settings) -> (bool, bool, Option<String>) {
    let listened = settings.sources.contains_key(&trigger.source);
    let observed = trigger
        .meta
        .get("observed")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let row = trigger
        .meta
        .get("row")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    (listened, observed, row)
}
fn participating(thread: &Thread, tickets: &[Ticket]) -> bool {
    let ctx: Vec<&str> = thread.context.as_ref().map(|c| c.iter().map(|s| s.as_str()).collect()).unwrap_or_default();
    thread
        .subscriptions
        .iter()
        .any(|id| !ctx.contains(&id.as_str()))
        || tickets.iter().any(|t| !ctx.contains(&t.id.as_str()))
}

#[derive(Debug, Clone)]
pub struct GateResult {
    pub admissible: bool,
    pub strength: Option<Strength>,
    pub basis: Option<MandateBasis>,
    pub reason: Option<String>,
}

/// The gate, exactly following TS `policy.gate` (human + machine branches).
pub fn gate(settings: &Settings, trigger: &Trigger, thread: &Thread, tickets: &[Ticket]) -> GateResult {
    let mode = &settings.eval_mode;
    if !trigger.human {
        let bot = if trigger.actor.id.is_empty() { None } else { listed_bot(settings, &trigger.actor) };
        if bot.is_none() {
            return GateResult { admissible: false, strength: None, basis: None, reason: Some("not_human".into()) };
        }
        let (listened, observed, row) = scope_of(trigger, settings);
        if !listened || !(trigger.addressed.is_some() || observed) {
            return GateResult { admissible: false, strength: None, basis: None, reason: Some("not_human".into()) };
        }
        if *mode == AcceptMode::Off {
            return GateResult { admissible: false, strength: None, basis: None, reason: Some("admission_ignored".into()) };
        }
        let bot = bot.unwrap();
        return GateResult {
            admissible: true,
            strength: Some(Strength::Inferred),
            basis: Some(MandateBasis::Bot {
                source: Some(trigger.source.clone()),
                sender: Some(bot.name.clone().unwrap_or_else(|| bot.id.clone())),
                rules: None,
                addressed: trigger.addressed.clone(),
                observation: Observation { mode: mode.clone() },
            }),
            reason: None,
        };
    }
    let allowed = allowed_sender(settings, &trigger.actor);
    let (listened, observed, row) = scope_of(trigger, settings);
    let row_ref = if listened { row.as_deref() } else { None };
    if let (Some(a), Some(addr)) = (allowed, &trigger.addressed) {
        return GateResult {
            admissible: true,
            strength: Some(Strength::Explicit),
            basis: Some(MandateBasis::Addressed {
                source: Some(trigger.source.clone()),
                sender: Some(a.name.clone().unwrap_or_else(|| a.id.clone())),
                rules: None,
                addressed: addr.clone(),
            }),
            reason: None,
        };
    }
    let cell = match admission_cell(allowed.is_some(), trigger.addressed.is_some()) {
        Some(c) => c,
        None => {
            return GateResult { admissible: false, strength: None, basis: None, reason: Some("no_mention".into()) }
        }
    };
    let receivable = trigger.addressed.is_some() || (listened && observed);
    let admission = if receivable { admission_of(settings, &trigger.source, row_ref, &cell) } else { "ignore".to_string() };
    if admission == "explicit" {
        return GateResult {
            admissible: true,
            strength: Some(Strength::Explicit),
            basis: Some(MandateBasis::Admission {
                source: Some(trigger.source.clone()),
                sender: allowed.map(|a| a.name.clone().unwrap_or_else(|| a.id.clone())),
                rules: None,
                cell,
                addressed: trigger.addressed.clone(),
                observation: None,
            }),
            reason: None,
        };
    }
    if !(trigger.addressed.as_deref() == Some("direct") && allowed.is_none())
        && (allowed.is_some() || *mode != AcceptMode::Off)
        && participating(thread, tickets)
    {
        return GateResult {
            admissible: true,
            strength: Some(if allowed.is_some() { Strength::Explicit } else { Strength::Inferred }),
            basis: Some(MandateBasis::Followup {
                source: Some(trigger.source.clone()),
                sender: allowed.map(|a| a.name.clone().unwrap_or_else(|| a.id.clone())),
                rules: None,
            }),
            reason: None,
        };
    }
    if admission == "inferred" && *mode != AcceptMode::Off {
        return GateResult {
            admissible: true,
            strength: Some(Strength::Inferred),
            basis: Some(MandateBasis::Admission {
                source: Some(trigger.source.clone()),
                sender: allowed.map(|a| a.name.clone().unwrap_or_else(|| a.id.clone())),
                rules: None,
                cell,
                addressed: trigger.addressed.clone(),
                observation: Some(Observation { mode: mode.clone() }),
            }),
            reason: None,
        };
    }
    let reason = if allowed.is_none() {
        "not_a_member"
    } else if receivable && admission == "ignore" {
        "admission_ignored"
    } else {
        "no_mention"
    };
    GateResult { admissible: false, strength: None, basis: None, reason: Some(reason.into()) }
}

pub fn admits_inferred(settings: &Settings, rules: &[String]) -> bool {
    settings.eval_mode == AcceptMode::Auto
        && !rules.is_empty()
        && rules.iter().all(|id| settings.rules.iter().any(|r| r.enabled && &r.id == id))
}

/// Revalidate a mandate against current settings (TS `revalidate`, Eval path).
pub fn revalidate(settings: &Settings, mandate: &Mandate) -> bool {
    let mode = &settings.eval_mode;
    let basis = &mandate.basis;
    let rules_on = |rules: Option<&Vec<String>>| -> bool {
        rules.map(|rs| rs.iter().all(|id| settings.rules.iter().any(|r| r.enabled && &r.id == id))).unwrap_or(true)
    };
    match basis {
        MandateBasis::Followup { .. } => {
            mandate.strength == Strength::Explicit || (*mode == AcceptMode::Auto && rules_on(basis.rules()))
        }
        MandateBasis::Bot { .. } => {
            if listed_bot(settings, &mandate.actor).is_none() {
                return false;
            }
            // observed bot mandates hold only in auto with rules on
            *mode == AcceptMode::Auto && rules_on(basis.rules())
        }
        MandateBasis::Admission { cell, observation, .. } => {
            let listed = allowed_sender(settings, &mandate.actor).is_some();
            if *cell == AdmissionCell::UnlistedAddressed && listed {
                return true;
            }
            let now_cell = if *cell == AdmissionCell::UnlistedUnaddressed && listed {
                AdmissionCell::ListedUnaddressed
            } else {
                cell.clone()
            };
            if now_cell == AdmissionCell::ListedUnaddressed && !listed {
                return false;
            }
            // scope row is not tracked in this port's mandate (no row stored);
            // use default admission for the cell.
            let admission = admission_default(&now_cell);
            let strength_rank = |s: &str| match s { "ignore" => 0, "inferred" => 1, "explicit" => 2, _ => 0 };
            let want = match mandate.strength { Strength::Explicit => 2, Strength::Inferred => 1 };
            if strength_rank(admission) < want {
                return false;
            }
            if mandate.strength == Strength::Explicit {
                return true;
            }
            if observation.is_some() {
                return *mode == AcceptMode::Auto && rules_on(basis.rules());
            }
            true
        }
        _ => {
            // addressed / listed: sender must still be allowed, except
            // addressed explicit which TS checks via the else-branch.
            if matches!(basis, MandateBasis::Addressed { .. }) {
                return allowed_sender(settings, &mandate.actor).is_some()
                    || mandate.strength == Strength::Explicit;
            }
            allowed_sender(settings, &mandate.actor).is_some()
        }
    }
}

/// Assessment validation (TS `assessmentOutcome` core): accept needs a
/// settled direction, >=1 matched enabled rule, intent evidence; inferred
/// accept outside `auto` becomes an `ignore` with would_accept audit.
#[derive(Debug, Clone)]
pub struct Assessment {
    pub decision: String, // accept|ignore|unavailable
    pub rule_assessments: Vec<(String, bool, String)>, // (rule_id, matched, reason)
    pub direction_allowed: bool,
    pub title: String,
    pub scope: String,
    pub reason: String,
    pub intent_evidence: Option<String>,
    pub acknowledgement: Option<String>,
    pub worktree_name: Option<String>,
}
#[derive(Debug, Clone)]
pub struct AssessmentOutcome {
    pub kind: String,
    pub reason: String,
    pub title: Option<String>,
    pub scope: Option<String>,
    pub rules: Vec<String>,
    pub would_accept: bool,
}
pub fn assessment_outcome(settings: &Settings, a: &Assessment, strength: &Strength) -> Result<AssessmentOutcome, String> {
    let enabled: Vec<&Rule> = settings.rules.iter().filter(|r| r.enabled).collect();
    if a.rule_assessments.len() != enabled.len()
        || {
            let mut ids: Vec<&str> = a.rule_assessments.iter().map(|x| x.0.as_str()).collect();
            ids.sort();
            ids.dedup();
            ids.len() != a.rule_assessments.len()
        }
        || enabled.iter().any(|r| !a.rule_assessments.iter().any(|x| x.0 == r.id))
    {
        return Err("Exactly one assessment for every enabled rule is required".into());
    }
    let matches: Vec<String> = a.rule_assessments.iter().filter(|x| x.1).map(|x| x.0.clone()).collect();
    if a.decision != "accept" {
        return Ok(AssessmentOutcome { kind: a.decision.clone(), reason: a.reason.clone(), title: None, scope: None, rules: matches, would_accept: false });
    }
    if !a.direction_allowed {
        return Ok(AssessmentOutcome { kind: "ignore".into(), reason: "directionUnsettled".into(), title: None, scope: None, rules: matches, would_accept: false });
    }
    if matches.is_empty() {
        return Ok(AssessmentOutcome { kind: "ignore".into(), reason: "noRuleMatched".into(), title: None, scope: None, rules: matches, would_accept: false });
    }
    if a.intent_evidence.is_none() {
        return Err("Acceptance requires intent_evidence".into());
    }
    if *strength == Strength::Inferred && settings.eval_mode != AcceptMode::Auto {
        return Ok(AssessmentOutcome {
            kind: "ignore".into(),
            reason: a.reason.clone(),
            title: Some(a.title.clone()),
            scope: Some(a.scope.clone()),
            rules: matches,
            would_accept: true,
        });
    }
    Ok(AssessmentOutcome {
        kind: "accept".into(),
        reason: a.reason.clone(),
        title: Some(a.title.clone()),
        scope: Some(a.scope.clone()),
        rules: matches,
        would_accept: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn settings_with(alice: bool) -> Settings {
        let mut s = Settings::default();
        let mut src = SourcePolicy::default();
        if alice {
            src.allowed.push(Allowed { id: "alice".into(), name: None });
        }
        src.enabled = true;
        s.sources.insert("github".into(), src);
        s
    }
    fn trig(actor: &str, addressed: Option<&str>, human: bool) -> Trigger {
        Trigger {
            id: "t1".into(),
            source: "github".into(),
            threads: vec![ThreadRef { source: "github".into(), id: "th1".into() }],
            actor: ActorRef { source: "github".into(), id: actor.into() },
            human,
            addressed: addressed.map(|s| s.to_string()),
            at: now_iso(),
            text: "fix typo".into(),
            channel: None,
            url: None,
            meta: serde_json::json!({}),
        }
    }
    fn thread() -> Thread {
        Thread {
            key: "github:th1".into(),
            r#ref: ThreadRef { source: "github".into(), id: "th1".into() },
            created_at: now_iso(),
            last_at: None,
            subscriptions: vec![],
            context: None,
            correlated: vec![],
            pending: vec![],
            evaluating: None,
            expired: None,
            meta: serde_json::json!({}),
        }
    }
    #[test]
    fn listed_addressed_is_explicit() {
        let s = settings_with(true);
        let g = gate(&s, &trig("Alice", Some("mention"), true), &thread(), &[]);
        assert!(g.admissible && g.strength == Some(Strength::Explicit));
    }
    #[test]
    fn unlisted_unaddressed_ignored_by_default() {
        let s = settings_with(true);
        let g = gate(&s, &trig("mallory", None, true), &thread(), &[]);
        assert!(!g.admissible && g.reason.as_deref() == Some("not_a_member"));
    }
    #[test]
    fn listed_unaddressed_inferred_in_observe() {
        let s = settings_with(true);
        let g = gate(&s, &trig("alice", None, true), &thread(), &[]);
        // receivable=false (not addressed, not observed) -> ignore per TS
        assert!(!g.admissible);
        let mut t = trig("alice", None, true);
        t.meta = serde_json::json!({"observed": true});
        let g2 = gate(&s, &t, &thread(), &[]);
        assert!(g2.admissible && g2.strength == Some(Strength::Inferred));
    }
    #[test]
    fn followup_beats_inferred_when_thread_has_ticket() {
        let mut s = settings_with(false);
        s.eval_mode = AcceptMode::Auto;
        let mut th = thread();
        th.subscriptions.push("ticket-1".into());
        let g = gate(&s, &trig("bob", None, true), &th, &[]);
        // unlisted, unaddressed, but thread participates + auto -> followup inferred
        assert!(g.admissible);
        assert!(matches!(g.basis, Some(MandateBasis::Followup { .. })));
    }
    #[test]
    fn policy_version_stable() {
        let s = settings_with(true);
        assert_eq!(policy_version(&s), policy_version(&s));
        assert_eq!(policy_version(&s).len(), 32);
    }
    #[test]
    fn assessment_requires_all_rules() {
        let s = Settings::default();
        let a = Assessment {
            decision: "accept".into(),
            rule_assessments: vec![("copy".into(), true, "typo".into())],
            direction_allowed: true,
            title: "x".into(),
            scope: "x".into(),
            reason: "ok".into(),
            intent_evidence: Some("explicit".into()),
            acknowledgement: None,
            worktree_name: None,
        };
        assert!(assessment_outcome(&s, &a, &Strength::Explicit).is_err());
    }
}
