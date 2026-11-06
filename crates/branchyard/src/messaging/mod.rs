//! One layer for every message between harnesses; see `docs/messaging.md`.
//!
//! A message in flight is an [`Envelope`]: who it is from and to
//! ([`Address`]), its registered [`KindSpec`] (a name, a [`Class`] that
//! says who may send it to whom and how it is delivered, and an optional
//! JSON Schema its `payload` must match), one line of `text`, an optional
//! `payload`, the message it replies to, a W3C `traceparent`, a deadline
//! and an idempotency key. Every surface (`by`, the MCP tools, the Python
//! module, `Delegate`, the server's routes) builds one and runs it through
//! the [`Pipeline`]: `authorize`, `validate`, `persist`, `observe`,
//! `deliver`, each a [`Stage`] a caller can add to. What comes out is the
//! stored [`Message`], whose `meta` carries what the legacy fields do not
//! (the kind, its version, the payload, the trace, the deadline), and
//! only when there is something to carry, so a message of a legacy kind
//! reads exactly as it did.
//!
//! The legacy paths are thin wrappers: `inbox::send` builds an envelope of
//! its class, and `handoff::relay` prepares its frozen commit and runs the
//! same pipeline with the `handoff` kind. The one new capability this
//! layer adds is a message between siblings ([`Class::Peer`]), of the
//! built-in `note` kind or a kind the embedding program registered with
//! [`crate::Yard::register_kind`], delivered at the recipient's next turn.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::record::Recorder;
use crate::{Activity, Error, Message, MessageKind, Observer, Yard, PERSON};

mod registry;
mod trace;

pub use registry::{KindSpec, Registry, DEFAULT_KIND};
pub use trace::check as check_traceparent;

/// The longest `text` a peer message may carry, in bytes: a brief entry's
/// limit.
pub const TEXT_MAX: usize = 16 * 1024;

/// The largest `payload` a peer message may carry, serialized, in bytes: a
/// result's limit.
pub const PAYLOAD_MAX: usize = 64 * 1024;

/// How many ways a payload fails its schema are named in a refusal.
const VIOLATIONS_SHOWN: usize = 8;

/// Where a message goes, or comes from.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Address {
    /// A branch, by name.
    Branch(String),
    /// The person who runs the repository, [`PERSON`].
    Person,
}

impl Address {
    /// The address `text` names: [`PERSON`] is the person, anything else a
    /// branch of that name (checked against the store when the message is
    /// sent, not here).
    pub fn parse(text: &str) -> Address {
        match text {
            PERSON => Address::Person,
            name => Address::Branch(name.to_owned()),
        }
    }

    /// The text [`Address::parse`] reads back: the branch's name, or
    /// [`PERSON`].
    pub fn as_str(&self) -> &str {
        match self {
            Address::Branch(name) => name,
            Address::Person => PERSON,
        }
    }

    /// The branch's name, when this is a branch.
    pub fn branch(&self) -> Option<&str> {
        match self {
            Address::Branch(name) => Some(name),
            Address::Person => None,
        }
    }
}

impl fmt::Display for Address {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Who may send a message to whom, and how it is delivered: the stored
/// `kind` column, which a reader of the inbox already knows. A registered
/// kind picks one.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Class {
    /// To the sender's parent (the person, for a branch nobody delegated);
    /// an `answer` is expected.
    Question,
    /// To the sender's parent; no answer is expected.
    Report,
    /// To the sender's parent, or further up when its rig seat allows.
    Escalation,
    /// From an ancestor to a descendant, or from the person in reply.
    Answer,
    /// A frozen result relayed to a coworker: an `answer` with a
    /// `handoff`, delivered at the recipient's next turn only.
    Handoff,
    /// Between siblings, the children of one parent; delivered at the
    /// recipient's next turn.
    Peer,
}

impl Class {
    /// The stored `kind` a message of this class has.
    pub fn message_kind(self) -> MessageKind {
        match self {
            Class::Question => MessageKind::Question,
            Class::Report => MessageKind::Report,
            Class::Escalation => MessageKind::Escalation,
            Class::Answer | Class::Handoff => MessageKind::Answer,
            Class::Peer => MessageKind::Peer,
        }
    }

    /// How a message of this class reaches its recipient unless its kind
    /// says otherwise.
    pub fn delivery(self) -> Delivery {
        match self {
            Class::Question | Class::Report | Class::Escalation | Class::Answer => Delivery::Steer,
            Class::Handoff | Class::Peer => Delivery::NextTurn,
        }
    }
}

/// When a message reaches its recipient's harness.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    /// Into the recipient's running turn through the yard's
    /// [`crate::DeliveryHook`], else at the start of its next turn.
    Steer,
    /// At the start of the recipient's next turn, never into a running
    /// one.
    NextTurn,
}

/// What a stored [`Message`] carries beyond its legacy fields. Present
/// only when there is something to carry, so a message of a legacy kind
/// with no payload and no trace serializes as it always did.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageMeta {
    /// The registered kind, when it is not the legacy one the `kind`
    /// field already names (`note`, or a custom kind).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The kind's schema version the payload was checked against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_version: Option<u32>,
    /// The payload, a JSON value the kind's schema accepted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    /// The W3C `traceparent` of the sender's turn, so the recipient's work
    /// on the message can join the trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
    /// Milliseconds since the Unix epoch after which the message should
    /// not be delivered. Recorded; not yet enforced (`docs/messaging.md`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deadline_ms: Option<u64>,
}

impl MessageMeta {
    /// Whether nothing is carried, so the field is left out.
    pub fn is_empty(&self) -> bool {
        *self == MessageMeta::default()
    }
}

/// One message in flight, before the store assigns its id.
#[derive(Clone, Debug, PartialEq)]
pub struct Envelope {
    /// The registered kind's name.
    pub kind: String,
    /// The sender.
    pub from: Address,
    /// The recipient.
    pub to: Address,
    /// The one line every surface shows.
    pub text: String,
    /// A JSON value the kind's schema checks.
    pub payload: Option<Value>,
    /// The message this replies to.
    pub correlation: Option<u64>,
    /// A W3C `traceparent`, checked when given.
    pub trace: Option<String>,
    /// After this moment the message should not be delivered.
    pub deadline_ms: Option<u64>,
    /// A key the sender chose: a repeat with the same key returns the
    /// message first sent with it. Scoped to the sender when stored.
    pub idempotency_key: Option<String>,
}

impl Envelope {
    /// A message of `kind` from `from` to `to` saying `text`, with nothing
    /// else set.
    pub fn new(kind: &str, from: Address, to: Address, text: &str) -> Envelope {
        Envelope {
            kind: kind.to_owned(),
            from,
            to,
            text: text.to_owned(),
            payload: None,
            correlation: None,
            trace: None,
            deadline_ms: None,
            idempotency_key: None,
        }
    }

    /// The key as stored: scoped to the sender, so two branches using the
    /// same key never collide.
    pub fn scoped_key(&self) -> Option<String> {
        self.idempotency_key
            .as_ref()
            .map(|key| format!("{}/{key}", self.from))
    }

    /// The `meta` a stored message of this envelope carries, empty when
    /// the legacy fields say everything.
    pub fn meta(&self, spec: &KindSpec) -> MessageMeta {
        let legacy = spec.name == spec.class.message_kind().as_str();
        MessageMeta {
            kind: (!legacy).then(|| spec.name.clone()),
            schema_version: (!legacy).then_some(spec.version),
            payload: self.payload.clone(),
            trace: self.trace.clone(),
            deadline_ms: self.deadline_ms,
        }
    }
}

/// What the stages share while one envelope goes through the pipeline.
pub struct Context {
    /// The message in flight.
    pub envelope: Envelope,
    /// Its kind, from the registry.
    pub spec: KindSpec,
    /// The stored message, once `persist` has run.
    pub message: Option<Message>,
    /// Set by `persist` when the idempotency key named a message already
    /// sent: the later stages then do nothing again.
    pub repeated: bool,
    /// A frozen handoff, prepared by `handoff::relay`, whose commit is the
    /// persist step.
    pub(crate) handoff: Option<crate::handoff::Prepared>,
    pub(crate) observer: Option<Observer>,
}

impl Context {
    fn new(envelope: Envelope, spec: KindSpec, observer: Option<Observer>) -> Context {
        Context {
            envelope,
            spec,
            message: None,
            repeated: false,
            handoff: None,
            observer,
        }
    }
}

/// One step of the [`Pipeline`]. The built-in stages are named
/// `authorize`, `validate`, `persist`, `observe` and `deliver`; a caller's
/// stage goes before or after one of them ([`Pipeline::before`],
/// [`Pipeline::then`]) to redact, rate-limit or trace.
pub trait Stage: Send + Sync {
    /// The stage's name, as [`Pipeline::names`] lists it.
    fn name(&self) -> &'static str;
    /// Apply the stage to the message in `cx`; an error refuses the
    /// message, and nothing after this stage runs.
    fn apply(&self, yard: &Yard, cx: &mut Context) -> Result<(), Error>;
}

/// The stages a message goes through, in order.
#[derive(Clone)]
pub struct Pipeline {
    stages: Vec<Arc<dyn Stage>>,
}

impl fmt::Debug for Pipeline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.names()).finish()
    }
}

impl Default for Pipeline {
    fn default() -> Pipeline {
        Pipeline::standard()
    }
}

impl Pipeline {
    /// `authorize`, `validate`, `persist`, `observe`, `deliver`: what every
    /// yard runs. `observe` follows `persist` because the event it records
    /// names the store's id, and nothing may say a message exists before
    /// it is durable.
    pub fn standard() -> Pipeline {
        Pipeline {
            stages: vec![
                Arc::new(Authorize),
                Arc::new(Validate),
                Arc::new(Persist),
                Arc::new(Observe),
                Arc::new(Deliver),
            ],
        }
    }

    /// This pipeline with `stage` after every other.
    pub fn then(mut self, stage: Arc<dyn Stage>) -> Pipeline {
        self.stages.push(stage);
        self
    }

    /// This pipeline with `stage` just before the stage named `name`, or
    /// last when no stage has that name.
    pub fn before(mut self, name: &str, stage: Arc<dyn Stage>) -> Pipeline {
        let at = self
            .stages
            .iter()
            .position(|s| s.name() == name)
            .unwrap_or(self.stages.len());
        self.stages.insert(at, stage);
        self
    }

    /// The stages' names, in order.
    pub fn names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// Run every stage over `cx`, in order, and return the stored message.
    pub fn run(&self, yard: &Yard, cx: &mut Context) -> Result<Message, Error> {
        for stage in &self.stages {
            stage.apply(yard, cx)?;
        }
        cx.message
            .clone()
            .ok_or_else(|| Error::State("the pipeline ended without storing the message".into()))
    }
}

/// The registry and pipeline a yard sends messages with.
#[derive(Clone, Debug)]
pub struct Messaging {
    /// The kinds this yard knows.
    pub registry: Registry,
    /// The stages a message goes through.
    pub pipeline: Pipeline,
}

impl Default for Messaging {
    fn default() -> Messaging {
        Messaging {
            registry: Registry::builtin(),
            pipeline: Pipeline::standard(),
        }
    }
}

/// Send `envelope` from `yard`: look its kind up, then run the pipeline.
pub(crate) fn send(
    yard: &Yard,
    observer: Option<Observer>,
    envelope: Envelope,
) -> Result<Message, Error> {
    let messaging = yard.messaging();
    let spec = messaging.registry.get(&envelope.kind)?;
    let mut cx = Context::new(envelope, spec, observer);
    messaging.pipeline.run(yard, &mut cx)
}

/// Send a frozen handoff `prepared` by `handoff::relay`, as the `handoff`
/// kind: its own authority and validation, committed in one transaction.
pub(crate) fn send_handoff(
    yard: &Yard,
    envelope: Envelope,
    prepared: crate::handoff::Prepared,
) -> Result<Message, Error> {
    let messaging = yard.messaging();
    let spec = messaging.registry.get(&envelope.kind)?;
    let mut cx = Context::new(envelope, spec, None);
    cx.handoff = Some(prepared);
    messaging.pipeline.run(yard, &mut cx)
}

/// Who may send what to whom: the delegation tree for every class
/// ([`crate::inbox::authorize`], which also knows siblings), and a reply's
/// question must have been sent to the replier; a handoff's common
/// ancestor ([`crate::handoff`]).
struct Authorize;

impl Stage for Authorize {
    fn name(&self) -> &'static str {
        "authorize"
    }

    fn apply(&self, yard: &Yard, cx: &mut Context) -> Result<(), Error> {
        let store = yard.store();
        if let Some(prepared) = &cx.handoff {
            return prepared.authorize();
        }
        let (from, to) = (cx.envelope.from.as_str(), cx.envelope.to.as_str());
        crate::inbox::authorize(&store, from, cx.spec.class.message_kind(), to)?;
        if let Some(question_id) = cx.envelope.correlation {
            let question = store
                .backend()
                .message(question_id)?
                .ok_or(Error::UnknownMessage(question_id))?;
            if question.to != from {
                return Err(Error::Denied(format!(
                    "message #{question_id} was not sent to {from}"
                )));
            }
        }
        Ok(())
    }
}

/// The text, the payload against the kind's schema, the sizes, the trace's
/// form and the deadline; a handoff's frozen facts.
struct Validate;

impl Stage for Validate {
    fn name(&self) -> &'static str {
        "validate"
    }

    fn apply(&self, _yard: &Yard, cx: &mut Context) -> Result<(), Error> {
        if let Some(prepared) = &cx.handoff {
            return prepared.validate();
        }
        let envelope = &cx.envelope;
        let spec = &cx.spec;
        if envelope.text.trim().is_empty() {
            return Err(Error::Denied(format!(
                "a {} needs text",
                spec.display_kind()
            )));
        }
        if spec.class == Class::Peer {
            if envelope.text.len() > TEXT_MAX {
                return Err(Error::Denied(format!(
                    "a {} message's text is at most {TEXT_MAX} bytes; put the detail in an \
                     artifact and name it",
                    spec.name
                )));
            }
            let size = envelope.payload.as_ref().map_or(0, |p| p.to_string().len());
            if size > PAYLOAD_MAX {
                return Err(Error::Denied(format!(
                    "a {} message's payload is at most {PAYLOAD_MAX} bytes, not {size}",
                    spec.name
                )));
            }
        }
        match (spec.schema(), &envelope.payload) {
            (Some(_), None) => {
                return Err(Error::Denied(format!(
                    "a {} message needs a payload matching its schema: {}",
                    spec.name,
                    spec.schema_value()
                        .map(Value::to_string)
                        .unwrap_or_default()
                )));
            }
            (Some(schema), Some(payload)) => {
                let violations = schema.validate_at(payload, "payload");
                if !violations.is_empty() {
                    let shown: Vec<&String> = violations.iter().take(VIOLATIONS_SHOWN).collect();
                    return Err(Error::Denied(format!(
                        "the payload does not match the {} kind's schema: {}",
                        spec.name,
                        shown
                            .iter()
                            .map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join("; ")
                    )));
                }
            }
            (None, _) => {}
        }
        if let Some(trace) = &envelope.trace {
            trace::check(trace)?;
        }
        if let Some(deadline) = envelope.deadline_ms {
            let now = branchyard_support::time::now_ms();
            if deadline <= now {
                return Err(Error::Denied(format!(
                    "the deadline {} has passed (now {})",
                    branchyard_support::time::rfc3339(deadline),
                    branchyard_support::time::rfc3339(now)
                )));
            }
        }
        Ok(())
    }
}

/// The durable step: the message's row (or the handoff's transaction), or
/// the message an idempotency key already names.
struct Persist;

impl Stage for Persist {
    fn name(&self) -> &'static str {
        "persist"
    }

    fn apply(&self, yard: &Yard, cx: &mut Context) -> Result<(), Error> {
        let store = yard.store();
        if let Some(prepared) = &cx.handoff {
            let message = store.backend().commit_handoff(&prepared.commit)?;
            store.notify();
            cx.message = Some(message);
            return Ok(());
        }
        let envelope = &cx.envelope;
        let key = envelope.scoped_key();
        if let Some(key) = &key {
            if let Some(existing) = store.backend().message_by_key(key)? {
                let same = existing.from == envelope.from.as_str()
                    && existing.to == envelope.to.as_str()
                    && existing.text == envelope.text
                    && existing.kind == cx.spec.class.message_kind()
                    // Every peer kind is stored as the `peer` class, so the
                    // registered kind is compared on its own.
                    && existing.kind_name() == envelope.kind
                    && existing.meta.as_ref().and_then(|m| m.payload.as_ref())
                        == envelope.payload.as_ref();
                if !same {
                    return Err(Error::Denied(format!(
                        "key {} already names message #{} from {} to {}: a key names one message",
                        envelope.idempotency_key.as_deref().unwrap_or_default(),
                        existing.id,
                        existing.from,
                        existing.to
                    )));
                }
                cx.message = Some(existing);
                cx.repeated = true;
                return Ok(());
            }
        }
        let meta = envelope.meta(&cx.spec);
        let message = store.backend().send_message(
            &Message {
                id: 0,
                from: envelope.from.as_str().to_owned(),
                to: envelope.to.as_str().to_owned(),
                kind: cx.spec.class.message_kind(),
                text: envelope.text.clone(),
                in_reply_to: envelope.correlation,
                at_ms: 0,
                delivered: false,
                handoff: None,
                next_turn: cx.spec.delivery == Delivery::NextTurn,
                meta: (!meta.is_empty()).then_some(meta),
            },
            key.as_deref(),
        )?;
        cx.message = Some(message);
        Ok(())
    }
}

/// [`Activity::Message`] on the sender's and the recipient's log (the
/// person keeps none). A handoff's commit appended its events itself, in
/// its transaction; a repeat records nothing again.
struct Observe;

impl Stage for Observe {
    fn name(&self) -> &'static str {
        "observe"
    }

    fn apply(&self, yard: &Yard, cx: &mut Context) -> Result<(), Error> {
        if cx.repeated || cx.handoff.is_some() {
            return Ok(());
        }
        let Some(message) = &cx.message else {
            return Ok(());
        };
        let store = yard.store();
        // `authorize` refused `to == from`, so these are two distinct logs.
        for branch in [&message.from, &message.to]
            .into_iter()
            .filter(|b| *b != PERSON)
        {
            if let Ok(mut recorder) = Recorder::open(&store, branch, cx.observer.clone()) {
                branchyard_support::best_effort(
                    "record the activity",
                    recorder.record(Activity::Message(message.clone())),
                );
            }
        }
        Ok(())
    }
}

/// Into the recipient's running turn, for a kind delivered by steering;
/// otherwise the message waits for the recipient's next turn to start.
struct Deliver;

impl Stage for Deliver {
    fn name(&self) -> &'static str {
        "deliver"
    }

    fn apply(&self, yard: &Yard, cx: &mut Context) -> Result<(), Error> {
        if cx.repeated || cx.handoff.is_some() || cx.spec.delivery != Delivery::Steer {
            return Ok(());
        }
        let Some(message) = &cx.message else {
            return Ok(());
        };
        if let Some(to) = cx.envelope.to.branch() {
            crate::inbox::try_deliver_now(yard, &yard.store(), to, message);
        }
        Ok(())
    }
}

#[allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_address_round_trips_through_text() {
        assert_eq!(Address::parse("@person"), Address::Person);
        assert_eq!(Address::parse("kid"), Address::Branch("kid".into()));
        for text in ["@person", "kid"] {
            assert_eq!(Address::parse(text).to_string(), text);
        }
        assert_eq!(Address::Person.branch(), None);
        assert_eq!(Address::parse("kid").branch(), Some("kid"));
    }

    #[test]
    fn a_legacy_kind_carries_no_meta_unless_there_is_something_to_carry() {
        let registry = Registry::builtin();
        let question = registry.get("question").unwrap();
        let envelope = Envelope::new("question", Address::parse("kid"), Address::Person, "why?");
        assert!(envelope.meta(&question).is_empty());
        let mut traced = envelope.clone();
        traced.trace = Some("00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01".into());
        let meta = traced.meta(&question);
        assert!(!meta.is_empty());
        assert_eq!(meta.kind, None, "the legacy kind is the kind field's");
        assert!(meta.trace.is_some());
        let note = registry.get(DEFAULT_KIND).unwrap();
        let peer = Envelope::new("note", Address::parse("a"), Address::parse("b"), "hi");
        let meta = peer.meta(&note);
        assert_eq!(meta.kind.as_deref(), Some("note"));
        assert_eq!(meta.schema_version, Some(1));
        // The field is left out of the JSON while empty.
        let message = Message {
            id: 1,
            from: "kid".into(),
            to: "parent".into(),
            kind: MessageKind::Question,
            text: "why?".into(),
            in_reply_to: None,
            at_ms: 1,
            delivered: false,
            handoff: None,
            next_turn: false,
            meta: None,
        };
        let text = serde_json::to_string(&message).unwrap();
        assert!(!text.contains("meta"), "{text}");
        assert!(!text.contains("next_turn"), "{text}");
    }

    #[test]
    fn a_key_is_scoped_to_its_sender() {
        let mut envelope = Envelope::new("note", Address::parse("a"), Address::parse("b"), "hi");
        assert_eq!(envelope.scoped_key(), None);
        envelope.idempotency_key = Some("step-1".into());
        assert_eq!(envelope.scoped_key().as_deref(), Some("a/step-1"));
    }

    #[test]
    fn each_class_names_its_stored_kind_and_delivery() {
        assert_eq!(Class::Handoff.message_kind(), MessageKind::Answer);
        assert_eq!(Class::Peer.message_kind(), MessageKind::Peer);
        assert_eq!(Class::Question.delivery(), Delivery::Steer);
        assert_eq!(Class::Peer.delivery(), Delivery::NextTurn);
        assert_eq!(Class::Handoff.delivery(), Delivery::NextTurn);
        assert_eq!(serde_json::to_value(Class::Peer).unwrap(), json!("peer"));
    }

    struct Named(&'static str);

    impl Stage for Named {
        fn name(&self) -> &'static str {
            self.0
        }
        fn apply(&self, _yard: &Yard, _cx: &mut Context) -> Result<(), Error> {
            Ok(())
        }
    }

    #[test]
    fn the_standard_pipeline_is_ordered_and_takes_stages_before_and_after() {
        let standard = Pipeline::standard();
        assert_eq!(
            standard.names(),
            ["authorize", "validate", "persist", "observe", "deliver"]
        );
        let extended = Pipeline::standard()
            .before("persist", Arc::new(Named("redact")))
            .before("authorize", Arc::new(Named("rate_limit")))
            .then(Arc::new(Named("trace")))
            .before("nowhere", Arc::new(Named("last")));
        assert_eq!(
            extended.names(),
            [
                "rate_limit",
                "authorize",
                "validate",
                "redact",
                "persist",
                "observe",
                "deliver",
                "trace",
                "last"
            ]
        );
        assert_eq!(format!("{extended:?}").matches("redact").count(), 1);
        assert_eq!(Messaging::default().pipeline.names(), standard.names());
    }
}
