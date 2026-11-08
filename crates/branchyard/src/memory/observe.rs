//! Observations: small typed scrubbed records derived from a turn's event
//! log at the turn's end, the raw material consolidation reads
//! (`docs/memory.md`, section 2). They are not memories and no harness ever
//! sees one.
//!
//! An observation is derived once per turn, by the engine that ran it,
//! from the events of that turn only ([`derive()`]), scrubbed before it is
//! stored ([`super::scrub`]) and written with an idempotency key over
//! `(branch, incarnation, turn, kind, text)` ([`NewObservation::key`]), so
//! a turn settled twice yields one set and a branch removed and recreated
//! (a new incarnation) is a new source. It names the events it came from by
//! `(incarnation, seq)`, the unique key of the `events` table, and keeps
//! its own text, so a removed branch leaves a reference that no longer
//! resolves, not a lost observation.

use std::collections::BTreeMap;

use branchyard_harness::target::{ScrubKind, TargetKind};
use branchyard_harness::Event;
use serde::{Deserialize, Serialize};

use crate::fleet::{FleetActivity, TaskKind};
use crate::goal::GoalActivity;
use crate::knowledge;
use crate::plan::PlanActivity;
use crate::pull_request::PullRequestActivity;
use crate::reported::ReportedState;
use crate::{
    prompt_digest, Activity, BranchStatus, DecisionSource, Error, Feedback, FeedbackKind,
    FeedbackSource, MessageKind, RecordedEvent, Yard,
};

/// The most characters an observation's text holds.
pub const TEXT_MAX: usize = 4_000;

/// How many times a command must repeat in one turn to be observed.
pub const REPEATED_MIN: usize = 2;

/// The tenant of a session without a server.
pub const LOCAL_TENANT: &str = "local";

/// One event of a branch's log, as the `events` table keys it.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EventRef {
    /// The branch incarnation the event belongs to.
    pub incarnation: i64,
    /// Its sequence number within the incarnation.
    pub seq: u64,
}

/// Whose turns an observation may feed: the session's own visibility.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "visibility", rename_all = "snake_case")]
pub enum Visibility {
    /// A shared session: the observation feeds the repository's memory.
    #[default]
    Shared,
    /// A private session: it feeds only this person's memory.
    Private {
        /// The person.
        subject: String,
    },
}

impl Visibility {
    /// The subject of a private observation.
    pub fn subject(&self) -> Option<&str> {
        match self {
            Visibility::Shared => None,
            Visibility::Private { subject } => Some(subject),
        }
    }
}

/// Whether a correction's author belongs to the repository.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Association {
    /// A committer of the repository, a principal of the tenant, or a name
    /// `[memory] members` lists.
    Member,
    /// Anyone else: a public pull request's comments are text anyone can
    /// write, and are kept out of candidates by default.
    External,
}

/// Who wrote a correction.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// The name as the source gave it: a forge login, a git author.
    pub name: String,
    /// Whether the name is a member's.
    pub association: Association,
}

/// What kind of thing was observed.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObservationKind {
    /// A send after the first, a steer, a review or pull-request comment:
    /// the knowledge extractor's inputs.
    Correction,
    /// A `--check` or CI check that failed, with the command and the tail
    /// of its output.
    CheckFailed,
    /// A permission request the policy refused: the tool and the rule,
    /// never the arguments.
    ToolDenied,
    /// A goal verdict not met: what was missing.
    GoalMissing,
    /// A judge's reasons for or against a candidate.
    JudgeVerdict,
    /// A plan rejected, with the reason.
    PlanRejected,
    /// An escalation message, or a `blocked` or `failure` status report.
    Escalation,
    /// How the branch ended: merged, failed, over budget, discarded.
    Outcome,
    /// The same shell command run twice or more in one turn.
    RepeatedCommand,
}

impl ObservationKind {
    /// Every kind.
    pub const ALL: [ObservationKind; 9] = [
        ObservationKind::Correction,
        ObservationKind::CheckFailed,
        ObservationKind::ToolDenied,
        ObservationKind::GoalMissing,
        ObservationKind::JudgeVerdict,
        ObservationKind::PlanRejected,
        ObservationKind::Escalation,
        ObservationKind::Outcome,
        ObservationKind::RepeatedCommand,
    ];

    /// The kind as serialized: `correction`, `check_failed`, ...
    pub fn as_str(self) -> &'static str {
        match self {
            ObservationKind::Correction => "correction",
            ObservationKind::CheckFailed => "check_failed",
            ObservationKind::ToolDenied => "tool_denied",
            ObservationKind::GoalMissing => "goal_missing",
            ObservationKind::JudgeVerdict => "judge_verdict",
            ObservationKind::PlanRejected => "plan_rejected",
            ObservationKind::Escalation => "escalation",
            ObservationKind::Outcome => "outcome",
            ObservationKind::RepeatedCommand => "repeated_command",
        }
    }

    /// Whether two or more of this kind in a cluster make a candidate
    /// memory (`docs/memory.md`, section 4, step 4).
    pub fn proposes(self) -> bool {
        matches!(
            self,
            ObservationKind::Correction
                | ObservationKind::CheckFailed
                | ObservationKind::RepeatedCommand
        )
    }
}

impl std::str::FromStr for ObservationKind {
    type Err = Error;
    fn from_str(text: &str) -> Result<ObservationKind, Error> {
        ObservationKind::ALL
            .into_iter()
            .find(|kind| kind.as_str() == text)
            .ok_or_else(|| Error::State(format!("{text:?} is not an observation kind")))
    }
}

impl std::fmt::Display for ObservationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An observation before the store assigns its id.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewObservation {
    /// The session's tenant; [`LOCAL_TENANT`] without a server.
    pub tenant: String,
    /// The repository, as its store labels it.
    pub repo: String,
    /// The branch whose turn it came from.
    pub branch: String,
    /// The branch incarnation the events belong to.
    pub incarnation: i64,
    /// The turn, from 1.
    pub turn: u32,
    /// The events it came from; at least one.
    pub events: Vec<EventRef>,
    /// The session's visibility.
    #[serde(flatten)]
    pub visibility: Visibility,
    /// What kind of thing was observed.
    pub kind: ObservationKind,
    /// Scrubbed; at most [`TEXT_MAX`] characters.
    pub text: String,
    /// Repository paths it is about.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    /// The branch's task kind, when the record names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_kind: Option<TaskKind>,
    /// The harness that ran the turn.
    pub harness: String,
    /// The model it ran, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Who wrote a correction.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<Author>,
    /// Milliseconds since the Unix epoch, of the turn's end.
    pub at_ms: u64,
    /// What the scrubber removed, by kind, one entry per removal; never
    /// the value.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scrubbed: Vec<ScrubKind>,
}

impl NewObservation {
    /// The idempotency key: BLAKE3 of `(tenant, subject, branch,
    /// incarnation, turn, kind, text)`, so a turn settled twice yields one
    /// set, a new incarnation is a new source, and two tenants' sessions
    /// on one repository never share a row.
    pub fn key(&self) -> String {
        let mut hasher = blake3::Hasher::new();
        for part in [
            self.tenant.as_str(),
            self.visibility.subject().unwrap_or(""),
            self.branch.as_str(),
            &self.incarnation.to_string(),
            &self.turn.to_string(),
            self.kind.as_str(),
            &self.text,
        ] {
            hasher.update(part.as_bytes());
            hasher.update(&[0]);
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Whether a correction by this author is kept out of candidates.
    pub fn external(&self) -> bool {
        self.author
            .as_ref()
            .is_some_and(|a| a.association == Association::External)
    }
}

/// An observation as stored.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Assigned by the store, from 1.
    pub id: u64,
    /// Everything but the id.
    #[serde(flatten)]
    pub body: NewObservation,
}

impl std::ops::Deref for Observation {
    type Target = NewObservation;
    fn deref(&self) -> &NewObservation {
        &self.body
    }
}

/// What a turn's derivation knows besides the events.
#[derive(Clone, Debug)]
pub struct Context<'a> {
    /// The session's tenant.
    pub tenant: &'a str,
    /// The repository, as its store labels it.
    pub repo: &'a str,
    /// The branch.
    pub branch: &'a str,
    /// The branch incarnation the events belong to.
    pub incarnation: i64,
    /// The turn the events belong to, from 1.
    pub turn: u32,
    /// The session's visibility.
    pub visibility: &'a Visibility,
    /// The harness that ran the turn.
    pub harness: &'a str,
    /// The model it ran, when known.
    pub model: Option<&'a str>,
    /// The branch's task kind.
    pub task_kind: Option<TaskKind>,
    /// Lowercased names and emails that make an author a member.
    pub members: &'a [String],
    /// The turn's end, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// One observation before scrubbing: what [`derive()`] finds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Draft {
    /// What kind of thing was observed.
    pub kind: ObservationKind,
    /// The text, not yet scrubbed.
    pub text: String,
    /// Repository paths it is about.
    pub paths: Vec<String>,
    /// Who wrote a correction.
    pub author: Option<Author>,
    /// The `seq` of each event it came from, within the incarnation.
    pub seqs: Vec<u64>,
}

impl Draft {
    fn new(kind: ObservationKind, text: impl Into<String>, seq: u64) -> Draft {
        Draft {
            kind,
            text: text.into(),
            paths: Vec::new(),
            author: None,
            seqs: vec![seq],
        }
    }

    /// The observation this draft becomes once `text` was scrubbed.
    pub fn into_observation(
        self,
        ctx: &Context<'_>,
        text: String,
        scrubbed: Vec<ScrubKind>,
    ) -> NewObservation {
        NewObservation {
            tenant: ctx.tenant.to_owned(),
            repo: ctx.repo.to_owned(),
            branch: ctx.branch.to_owned(),
            incarnation: ctx.incarnation,
            turn: ctx.turn,
            events: self
                .seqs
                .iter()
                .map(|seq| EventRef {
                    incarnation: ctx.incarnation,
                    seq: *seq,
                })
                .collect(),
            visibility: ctx.visibility.clone(),
            kind: self.kind,
            text: cut(&text),
            paths: self.paths,
            task_kind: ctx.task_kind,
            harness: ctx.harness.to_owned(),
            model: ctx.model.map(str::to_owned),
            author: self.author,
            at_ms: ctx.at_ms,
            scrubbed,
        }
    }
}

/// `text` cut to [`TEXT_MAX`] characters at a line when it can.
fn cut(text: &str) -> String {
    let text = text.trim();
    if text.chars().count() <= TEXT_MAX {
        return text.to_owned();
    }
    let kept: String = text.chars().take(TEXT_MAX - 3).collect();
    let kept = match kept.rfind('\n') {
        Some(at) if at > TEXT_MAX / 2 => &kept[..at],
        _ => kept.as_str(),
    };
    format!("{}...", kept.trim_end())
}

/// The events of turn `turn` (from 1) in `events`: from its prompt to the
/// next one; `None` when the branch has run fewer turns. The feed is in
/// order of `seq`.
pub fn turn_events(events: &[(u64, RecordedEvent)], turn: u32) -> Option<&[(u64, RecordedEvent)]> {
    let mut starts = events
        .iter()
        .enumerate()
        .filter(|(_, (_, e))| matches!(e.activity, Activity::Prompt(_)))
        .map(|(at, _)| at);
    let start = starts.by_ref().nth(turn.checked_sub(1)? as usize)?;
    let end = starts.next().unwrap_or(events.len());
    Some(&events[start..end])
}

/// The author `name` as a member or not, given the lowercased members.
pub fn author(name: &str, members: &[String]) -> Author {
    let lower = name.trim().trim_start_matches('@').to_ascii_lowercase();
    let association = match members.iter().any(|m| m == &lower) {
        true => Association::Member,
        false => Association::External,
    };
    Author {
        name: name.trim().to_owned(),
        association,
    }
}

/// How a denial's source reads in an observation.
fn denial_source(source: &DecisionSource) -> String {
    match source {
        DecisionSource::Rule { pattern } => format!("rule {pattern}"),
        DecisionSource::Default => "the policy's default".into(),
        DecisionSource::Asked => "the person asked".into(),
        DecisionSource::Engine => "the engine".into(),
        DecisionSource::Approval { .. } => "the approval policy".into(),
    }
}

/// Derive the observations of turn `ctx.turn` from the branch's whole feed
/// (`events`, in `seq` order): deterministic, no model. The text is not
/// scrubbed yet.
pub fn derive(events: &[(u64, RecordedEvent)], ctx: &Context<'_>) -> Vec<Draft> {
    let Some(turn) = turn_events(events, ctx.turn) else {
        return Vec::new();
    };
    // Feedback is recorded just before the prompt that carries it, so it
    // may sit in the previous turn's range: pair by digest over the whole
    // feed, as the extractor does.
    let recorded: BTreeMap<&str, (u64, &Feedback)> = events
        .iter()
        .filter_map(|(seq, event)| match &event.activity {
            Activity::Feedback(feedback) => feedback
                .prompt
                .as_deref()
                .map(|d| (d, (*seq, feedback.as_ref()))),
            _ => None,
        })
        .collect();
    let mut drafts = Vec::new();
    let mut commands: BTreeMap<String, Vec<u64>> = BTreeMap::new();
    let mut turns_so_far = 0u32;
    let mut cost: Option<f64> = None;
    for (seq, event) in turn {
        let seq = *seq;
        match &event.activity {
            Activity::Prompt(prompt) => {
                turns_so_far = ctx.turn;
                if ctx.turn <= 1 {
                    continue;
                }
                corrections(prompt, seq, "send", &recorded, ctx, &mut drafts);
            }
            Activity::Steered { text, .. } => {
                corrections(text, seq, "steer", &recorded, ctx, &mut drafts);
            }
            Activity::Decision {
                tool,
                allowed: false,
                source,
                ..
            } => {
                drafts.push(Draft::new(
                    ObservationKind::ToolDenied,
                    format!("{tool} denied by {}", denial_source(source)),
                    seq,
                ));
            }
            Activity::PullRequest(activity) => {
                if let PullRequestActivity::Checked(check) = activity.as_ref() {
                    if !check.passed {
                        let tail = check
                            .output_tail
                            .lines()
                            .rev()
                            .take(40)
                            .collect::<Vec<_>>()
                            .into_iter()
                            .rev()
                            .collect::<Vec<_>>()
                            .join("\n");
                        let what = match check.timed_out {
                            true => "timed out",
                            false => "failed",
                        };
                        drafts.push(Draft::new(
                            ObservationKind::CheckFailed,
                            format!("check `{}` {what}\n{tail}", check.argv.join(" ")),
                            seq,
                        ));
                    }
                }
            }
            Activity::Goal(goal) => match goal.as_ref() {
                GoalActivity::Verdict {
                    met: false,
                    missing,
                    ..
                }
                | GoalActivity::Exhausted { missing, .. }
                    if !missing.is_empty() =>
                {
                    drafts.push(Draft::new(
                        ObservationKind::GoalMissing,
                        missing.join("\n"),
                        seq,
                    ));
                }
                _ => {}
            },
            Activity::Fleet(fleet) => {
                if let FleetActivity::Judged(mark) = fleet.as_ref() {
                    if let Some(reason) = mark.reason.as_deref().filter(|r| !r.trim().is_empty()) {
                        let verdict = match mark.picked {
                            true => "picked",
                            false => "not picked",
                        };
                        drafts.push(Draft::new(
                            ObservationKind::JudgeVerdict,
                            format!("{verdict}: {}", reason.trim()),
                            seq,
                        ));
                    }
                }
            }
            Activity::Plan(plan) => {
                if let PlanActivity::Rejected {
                    reason: Some(reason),
                    ..
                } = plan.as_ref()
                {
                    if !reason.trim().is_empty() {
                        drafts.push(Draft::new(
                            ObservationKind::PlanRejected,
                            reason.trim(),
                            seq,
                        ));
                    }
                }
            }
            Activity::Message(message)
                if message.kind == MessageKind::Escalation && message.from == ctx.branch =>
            {
                drafts.push(Draft::new(
                    ObservationKind::Escalation,
                    message.text.trim(),
                    seq,
                ));
            }
            Activity::StatusReport { state, summary, .. }
                if matches!(state, ReportedState::Blocked | ReportedState::Failure) =>
            {
                let what = match state {
                    ReportedState::Blocked => "blocked",
                    _ => "failure",
                };
                drafts.push(Draft::new(
                    ObservationKind::Escalation,
                    format!("{what}: {}", summary.trim()),
                    seq,
                ));
            }
            Activity::Harness(Event::ToolStarted {
                target: Some(target),
                ..
            }) if target.kind == TargetKind::Command => {
                commands.entry(target.value.clone()).or_default().push(seq);
            }
            Activity::TurnCost { cost_usd } => cost = Some(*cost_usd),
            Activity::Status(status) => {
                let text = match status {
                    BranchStatus::Merged { target, .. } => Some(format!("merged into {target}")),
                    BranchStatus::Failed { reason } => Some(format!("failed: {reason}")),
                    BranchStatus::BudgetExceeded { limit } => {
                        Some(format!("budget exceeded: {limit}"))
                    }
                    BranchStatus::Discarded { reason } => Some(format!("discarded: {reason}")),
                    _ => None,
                };
                if let Some(mut text) = text {
                    text.push_str(&format!(
                        " after {} turn{}",
                        turns_so_far.max(ctx.turn),
                        if ctx.turn == 1 { "" } else { "s" }
                    ));
                    if let Some(cost) = cost {
                        text.push_str(&format!(", this turn ${cost:.2}"));
                    }
                    drafts.push(Draft::new(ObservationKind::Outcome, text, seq));
                }
            }
            _ => {}
        }
    }
    for (command, seqs) in commands {
        if seqs.len() >= REPEATED_MIN {
            drafts.push(Draft {
                kind: ObservationKind::RepeatedCommand,
                text: format!("{command} ({} times)", seqs.len()),
                paths: Vec::new(),
                author: None,
                seqs,
            });
        }
    }
    drafts
}

/// The corrections in one prompt or steer: the person's own words, or the
/// comments and failed checks of the feedback it carried. Branchyard's own
/// prompts, continuation replies and inbox blocks are skipped, as the
/// extractor skips them.
fn corrections(
    prompt: &str,
    seq: u64,
    via: &str,
    recorded: &BTreeMap<&str, (u64, &Feedback)>,
    ctx: &Context<'_>,
    drafts: &mut Vec<Draft>,
) {
    let text = knowledge::own_text(prompt);
    if text.is_empty()
        || text.starts_with(knowledge::GENERATED)
        || text.starts_with(crate::inbox::OPEN_TAG)
    {
        return;
    }
    if let Some((feedback_seq, feedback)) = recorded.get(prompt_digest(text).as_str()) {
        let source = match feedback.source {
            FeedbackSource::PullRequest => "pull request",
            FeedbackSource::Review => "review",
        };
        for item in &feedback.items {
            let body = item.body.trim();
            if body.is_empty() {
                continue;
            }
            let seqs = vec![*feedback_seq, seq];
            match item.kind {
                FeedbackKind::Comment => {
                    if !knowledge::worth_proposing(body) {
                        continue;
                    }
                    drafts.push(Draft {
                        kind: ObservationKind::Correction,
                        text: format!("[{source}] {body}"),
                        paths: item.path.iter().cloned().collect(),
                        author: item.author.as_deref().map(|a| author(a, ctx.members)),
                        seqs,
                    });
                }
                FeedbackKind::Check => drafts.push(Draft {
                    kind: ObservationKind::CheckFailed,
                    text: body.to_owned(),
                    paths: Vec::new(),
                    author: None,
                    seqs,
                }),
            }
        }
        return;
    }
    // Feedback from before it was recorded as data, read from the prose as
    // the extractor reads it.
    if text.starts_with("Feedback on pull request #") {
        for (path, body) in knowledge::pull_request_feedback(text) {
            if knowledge::worth_proposing(&body) {
                drafts.push(Draft {
                    kind: ObservationKind::Correction,
                    text: format!("[pull request] {body}"),
                    paths: path.into_iter().collect(),
                    author: None,
                    seqs: vec![seq],
                });
            }
        }
        return;
    }
    let comments = knowledge::review_comments(text);
    if !comments.is_empty() {
        for (path, body) in comments {
            if knowledge::worth_proposing(&body) {
                drafts.push(Draft {
                    kind: ObservationKind::Correction,
                    text: format!("[review] {body}"),
                    paths: vec![path],
                    author: None,
                    seqs: vec![seq],
                });
            }
        }
        return;
    }
    if knowledge::worth_proposing(text) {
        drafts.push(Draft::new(
            ObservationKind::Correction,
            format!("[{via}] {text}"),
            seq,
        ));
    }
}

/// Lowercased author names and emails of the repository's commits, the
/// members a correction's author is matched against; plus `extra`, the
/// names `[memory] members` lists.
pub(crate) fn members(yard: &Yard, extra: &[String]) -> Vec<String> {
    let mut out: Vec<String> = extra.iter().map(|m| m.to_ascii_lowercase()).collect();
    if let Ok(log) = crate::git::run(
        yard.root(),
        &["log", "--all", "-n", "2000", "--format=%an%n%ae"],
    ) {
        out.extend(
            log.lines()
                .map(|l| l.trim().to_ascii_lowercase())
                .filter(|l| !l.is_empty()),
        );
    }
    out.sort();
    out.dedup();
    out
}

/// Every event of `branch`'s current incarnation, with its `seq`.
pub(crate) fn feed(yard: &Yard, branch: &str) -> Result<Vec<(u64, RecordedEvent)>, Error> {
    let store = yard.store();
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = store.backend().events_since(branch, cursor, 500)?;
        let Some((last, _)) = page.last() else {
            return Ok(events);
        };
        cursor = *last;
        events.extend(page);
    }
}

/// The current incarnation of `branch`.
pub(crate) fn incarnation_of(yard: &Yard, branch: &str) -> Result<i64, Error> {
    yard.store()
        .backend()
        .incarnation(branch)?
        .ok_or_else(|| Error::UnknownBranch(branch.to_owned()))
}

/// What the derivation of a turn did, for the caller's record.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observed {
    /// The ids of the observations stored (an idempotent repeat gives the
    /// same ids).
    pub ids: Vec<u64>,
    /// Observations dropped because more than a quarter of their text was
    /// secrets, by kind.
    pub dropped: Vec<ObservationKind>,
}

/// Derive, scrub and store the observations of turn `turn` of `branch`,
/// whose incarnation is `incarnation`, with `secrets` the exact values the
/// turn was provisioned with. Deterministic, idempotent per turn.
pub(crate) fn observe_turn(
    yard: &Yard,
    branch: &str,
    incarnation: i64,
    turn: u32,
    secrets: &[String],
) -> Result<Observed, Error> {
    let settings = yard.memory_settings();
    if !settings.capture {
        return Ok(Observed::default());
    }
    let store = yard.store();
    let record = store.read(branch)?;
    let events = feed(yard, branch)?;
    let plain: Vec<RecordedEvent> = events.iter().map(|(_, e)| e.clone()).collect();
    let members = members(yard, &settings.members);
    let tenant = record
        .actor
        .as_ref()
        .map_or_else(|| LOCAL_TENANT.to_owned(), |a| a.tenant.clone());
    let repo = store.label();
    let ctx = Context {
        tenant: &tenant,
        repo: &repo,
        branch,
        incarnation,
        turn,
        visibility: &settings.visibility,
        harness: &record.info.harness,
        model: record.info.model.as_deref(),
        task_kind: Some(knowledge::kind_of(&record, &plain)),
        members: &members,
        at_ms: branchyard_support::time::now_ms(),
    };
    let scrubber = super::scrub::Scrubber::new(secrets, &members);
    let mut observed = Observed::default();
    let mut fresh = Vec::new();
    for draft in derive(&events, &ctx) {
        match scrubber.scrub(&draft.text) {
            Some((text, scrubbed)) => fresh.push(draft.into_observation(&ctx, text, scrubbed)),
            None => observed.dropped.push(draft.kind),
        }
    }
    if !fresh.is_empty() {
        observed.ids = store.memory().observe(&fresh)?;
    }
    if !observed.dropped.is_empty() {
        let kinds: Vec<&str> = observed.dropped.iter().map(|k| k.as_str()).collect();
        crate::record::Recorder::open(&store, branch, None)?.record(Activity::Warning(format!(
            "memory: {} observation(s) of turn {turn} dropped ({}): more than a quarter of the \
             text was secrets",
            observed.dropped.len(),
            kinds.join(", ")
        )))?;
    }
    Ok(observed)
}

/// [`observe_turn`] at a turn's end, best-effort: it never changes what
/// happened to the branch.
pub(crate) fn on_turn_end(
    yard: &Yard,
    branch: &str,
    incarnation: i64,
    turn: u32,
    secrets: &[String],
) {
    branchyard_support::best_effort(
        "derive the turn's memory observations",
        observe_turn(yard, branch, incarnation, turn, secrets),
    );
}
