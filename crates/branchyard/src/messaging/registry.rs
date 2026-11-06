//! The kinds of message a yard knows: the built-in ones and those the
//! embedding program registers with a JSON Schema.

use std::collections::BTreeMap;

use serde_json::Value;

use super::{Class, Delivery};
use crate::json_schema::{Dialect, JsonSchema};
use crate::Error;

/// The kind `message` sends when none is named: a line of text to a
/// sibling, with any payload or none.
pub const DEFAULT_KIND: &str = "note";

/// The names of the built-in kinds, which no registration may take.
const BUILTIN: &[&str] = &[
    "question",
    "report",
    "escalation",
    "answer",
    "handoff",
    DEFAULT_KIND,
];

/// The longest kind name.
const NAME_MAX: usize = 64;

/// One kind of message: its name, its [`Class`] (who may send it to whom),
/// its delivery, its schema version and, optionally, the JSON Schema its
/// payload must match.
#[derive(Clone, Debug)]
pub struct KindSpec {
    /// The name every surface uses: `[a-z0-9._-]`, at most 64 characters.
    pub name: String,
    /// Who may send it to whom, and the stored `kind` it has.
    pub class: Class,
    /// How it reaches its recipient.
    pub delivery: Delivery,
    /// The version of its payload schema, stored with each message.
    pub version: u32,
    schema: Option<JsonSchema>,
}

impl PartialEq for KindSpec {
    fn eq(&self, other: &KindSpec) -> bool {
        self.name == other.name
            && self.class == other.class
            && self.delivery == other.delivery
            && self.version == other.version
            && self.schema_value() == other.schema_value()
    }
}

impl KindSpec {
    /// A kind named `name` of `class`, delivered as the class is, version
    /// 1, with no payload schema (any payload, or none).
    pub fn new(name: &str, class: Class) -> Result<KindSpec, Error> {
        check_name(name)?;
        Ok(KindSpec {
            name: name.to_owned(),
            class,
            delivery: class.delivery(),
            version: 1,
            schema: None,
        })
    }

    /// This kind with a payload schema, in the subset result contracts use
    /// (`docs/delegation.md#result-contract`); a keyword outside it is
    /// refused by name. A message of the kind must then carry a payload.
    pub fn with_schema(mut self, schema: Value) -> Result<KindSpec, Error> {
        self.schema = Some(
            JsonSchema::declared(schema, Dialect::Kind)
                .map_err(|why| Error::Denied(format!("kind {}: {why}", self.name)))?,
        );
        Ok(self)
    }

    /// This kind at schema `version`.
    pub fn with_version(mut self, version: u32) -> KindSpec {
        self.version = version;
        self
    }

    /// This kind delivered as `delivery` says.
    pub fn with_delivery(mut self, delivery: Delivery) -> KindSpec {
        self.delivery = delivery;
        self
    }

    /// The payload schema, when the kind has one.
    pub fn schema(&self) -> Option<&JsonSchema> {
        self.schema.as_ref()
    }

    /// The payload schema as given, when the kind has one.
    pub fn schema_value(&self) -> Option<&Value> {
        self.schema.as_ref().map(JsonSchema::value)
    }

    /// How a refusal names the kind: the legacy word for a legacy kind
    /// (`a question needs text`), else the kind's name.
    pub(crate) fn display_kind(&self) -> String {
        match self.class {
            Class::Peer => format!("{} message", self.name),
            class => class.message_kind().to_string(),
        }
    }
}

fn check_name(name: &str) -> Result<(), Error> {
    let valid = !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'));
    match valid {
        true => Ok(()),
        false => Err(Error::Denied(format!(
            "a message kind is named with [a-z0-9._-], at most {NAME_MAX} characters, not {name:?}"
        ))),
    }
}

/// The kinds a yard knows, by name.
#[derive(Clone, Debug, PartialEq)]
pub struct Registry {
    kinds: BTreeMap<String, KindSpec>,
}

impl Registry {
    /// The built-in kinds: `question`, `report`, `escalation`, `answer`,
    /// `handoff`, and `note` (class `peer`, any payload).
    pub fn builtin() -> Registry {
        let mut kinds = BTreeMap::new();
        for (name, class) in [
            ("question", Class::Question),
            ("report", Class::Report),
            ("escalation", Class::Escalation),
            ("answer", Class::Answer),
            ("handoff", Class::Handoff),
            (DEFAULT_KIND, Class::Peer),
        ] {
            kinds.insert(
                name.to_owned(),
                KindSpec {
                    name: name.to_owned(),
                    class,
                    delivery: class.delivery(),
                    version: 1,
                    schema: None,
                },
            );
        }
        Registry { kinds }
    }

    /// Add `spec`. A built-in name, a name already registered, or the
    /// `handoff` class (whose message is a frozen result only
    /// `handoff::relay` can prepare) is refused.
    pub fn register(&mut self, spec: KindSpec) -> Result<(), Error> {
        if BUILTIN.contains(&spec.name.as_str()) {
            return Err(Error::Denied(format!(
                "{} is a built-in message kind; register another name",
                spec.name
            )));
        }
        if spec.class == Class::Handoff {
            return Err(Error::Denied(format!(
                "kind {}: a handoff is prepared by `by handoff`, not registered",
                spec.name
            )));
        }
        if self.kinds.contains_key(&spec.name) {
            return Err(Error::Denied(format!(
                "message kind {} is registered already",
                spec.name
            )));
        }
        self.kinds.insert(spec.name.clone(), spec);
        Ok(())
    }

    /// The kind named `name`, or a refusal that lists the names there are.
    pub fn get(&self, name: &str) -> Result<KindSpec, Error> {
        self.kinds.get(name).cloned().ok_or_else(|| {
            Error::Denied(format!(
                "no message kind named {name:?}; the kinds are {}",
                self.names().join(", ")
            ))
        })
    }

    /// Every kind's name, sorted.
    pub fn names(&self) -> Vec<String> {
        self.kinds.keys().cloned().collect()
    }

    /// Every kind, by name.
    pub fn kinds(&self) -> Vec<KindSpec> {
        self.kinds.values().cloned().collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)] // tests: a panic is the failure report
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_builtin_kinds_are_there_and_a_custom_one_registers_by_schema() {
        let mut registry = Registry::builtin();
        assert_eq!(
            registry.names(),
            [
                "answer",
                "escalation",
                "handoff",
                "note",
                "question",
                "report"
            ]
        );
        let note = registry.get("note").unwrap();
        assert_eq!(
            (note.class, note.delivery),
            (Class::Peer, Delivery::NextTurn)
        );
        assert!(note.schema().is_none());
        let step = KindSpec::new("step", Class::Peer)
            .unwrap()
            .with_schema(json!({"type": "object", "required": ["n"], "properties": {"n": {"type": "integer"}}}))
            .unwrap()
            .with_version(2);
        registry.register(step.clone()).unwrap();
        let found = registry.get("step").unwrap();
        assert_eq!(found, step);
        assert_eq!(found.version, 2);
        assert!(found.schema().unwrap().accepts(&json!({"n": 1})));
        assert!(!found.schema().unwrap().accepts(&json!({"n": "one"})));
        assert_eq!(registry.kinds().len(), 7);
    }

    #[test]
    fn a_bad_registration_is_refused_by_name() {
        let mut registry = Registry::builtin();
        let denied = |result: Result<(), Error>, needle: &str| match result {
            Err(Error::Denied(why)) => assert!(why.contains(needle), "{why}"),
            other => panic!("{other:?}"),
        };
        denied(
            registry.register(KindSpec::new("question", Class::Peer).unwrap()),
            "built-in",
        );
        denied(
            registry.register(KindSpec::new("frozen", Class::Handoff).unwrap()),
            "not registered",
        );
        registry
            .register(KindSpec::new("twice", Class::Peer).unwrap())
            .unwrap();
        denied(
            registry.register(KindSpec::new("twice", Class::Report).unwrap()),
            "registered already",
        );
        for name in ["", "Upper", "with space", &"x".repeat(65)] {
            denied(KindSpec::new(name, Class::Peer).map(|_| ()), "[a-z0-9._-]");
        }
        // A keyword outside the subset is refused where it stands.
        let refused = KindSpec::new("ref", Class::Peer)
            .unwrap()
            .with_schema(json!({"$ref": "#/x"}))
            .map(|_| ());
        denied(refused, "$ref");
        let no_format = KindSpec::new("fmt", Class::Peer)
            .unwrap()
            .with_schema(json!({"type": "string", "format": "uri"}))
            .map(|_| ());
        denied(no_format, "format");
        denied(
            registry.get("nothing").map(|_| ()),
            "no message kind named \"nothing\"; the kinds are answer, escalation",
        );
    }
}
