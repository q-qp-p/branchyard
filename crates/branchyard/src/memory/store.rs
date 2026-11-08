//! Where observations live, per scope, with the watermark consolidation
//! (M3) will read them from: one trait, `MemoryBackend`, on SQLite and
//! PostgreSQL with one conformance case each (`docs/memory.md`, section 3). Every row carries
//! a tenant: a served repository can be on several tenants' allowlists, and
//! a scope without one would make tenant A's corrections tenant B's memory.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::observe::{NewObservation, Observation};
use crate::Error;

/// The owner, and the audience, of a memory.
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum Scope {
    /// The team memory of one repository, on one tenant.
    Repo {
        /// The tenant whose sessions fed it.
        tenant: String,
        /// The repository, as its store labels it.
        repo: String,
    },
    /// One person's memory: read only by that person's turns and `by
    /// memory` run as them.
    Person {
        /// The tenant the person belongs to.
        tenant: String,
        /// The person.
        subject: String,
        /// A repository the memory is about; `None` for all of them.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        repo: Option<String>,
    },
}

impl Scope {
    /// The scope as the `memory_watermarks` and `memory_runs` tables key
    /// it: `repo:<tenant>:<repo>` or `person:<tenant>:<subject>[:<repo>]`.
    pub fn key(&self) -> String {
        match self {
            Scope::Repo { tenant, repo } => format!("repo:{tenant}:{repo}"),
            Scope::Person {
                tenant,
                subject,
                repo,
            } => match repo {
                Some(repo) => format!("person:{tenant}:{subject}:{repo}"),
                None => format!("person:{tenant}:{subject}"),
            },
        }
    }

    /// The scope a key names; see [`Scope::key`].
    pub fn parse_key(key: &str) -> Result<Scope, Error> {
        let bad = || Error::State(format!("{key:?} is not a memory scope"));
        let (kind, rest) = key.split_once(':').ok_or_else(bad)?;
        match kind {
            "repo" => {
                let (tenant, repo) = rest.split_once(':').ok_or_else(bad)?;
                Ok(Scope::Repo {
                    tenant: tenant.to_owned(),
                    repo: repo.to_owned(),
                })
            }
            "person" => {
                let mut parts = rest.splitn(3, ':');
                let tenant = parts.next().ok_or_else(bad)?;
                let subject = parts.next().ok_or_else(bad)?;
                Ok(Scope::Person {
                    tenant: tenant.to_owned(),
                    subject: subject.to_owned(),
                    repo: parts.next().map(str::to_owned),
                })
            }
            _ => Err(bad()),
        }
    }

    /// The tenant the scope belongs to.
    pub fn tenant(&self) -> &str {
        match self {
            Scope::Repo { tenant, .. } | Scope::Person { tenant, .. } => tenant,
        }
    }

    /// The person a `Person` scope belongs to.
    pub fn subject(&self) -> Option<&str> {
        match self {
            Scope::Repo { .. } => None,
            Scope::Person { subject, .. } => Some(subject),
        }
    }

    /// `repo` or `person`.
    pub fn kind(&self) -> &'static str {
        match self {
            Scope::Repo { .. } => "repo",
            Scope::Person { .. } => "person",
        }
    }

    /// One line for people: `the team's memory of app` or `ana's memory`.
    pub fn describe(&self) -> String {
        match self {
            Scope::Repo { repo, .. } => format!("the team's memory of {repo}"),
            Scope::Person { subject, .. } => format!("{subject}'s memory"),
        }
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.key())
    }
}

/// The `(tenant, subject)` filter of a scope's observations: a `Repo`
/// scope reads shared observations (no subject), a `Person` scope the
/// subject's.
pub(crate) fn scope_filter(scope: &Scope) -> (&str, Option<&str>, Option<&str>) {
    match scope {
        Scope::Repo { tenant, repo } => (tenant, None, Some(repo)),
        Scope::Person {
            tenant,
            subject,
            repo,
        } => (tenant, Some(subject), repo.as_deref()),
    }
}

/// Observations and their watermarks, per repository store.
pub(crate) trait MemoryBackend: Send + Sync + fmt::Debug {
    /// Store each observation once, by its key; the ids, in order (an
    /// observation already stored gives its existing id).
    fn observe(&self, obs: &[NewObservation]) -> Result<Vec<u64>, Error>;
    /// The observations of `scope` with an id above `watermark`, oldest
    /// first, at most `limit`.
    fn observations_since(
        &self,
        scope: &Scope,
        watermark: u64,
        limit: usize,
    ) -> Result<Vec<Observation>, Error>;
    /// The scope's watermark: the last observation id consolidation
    /// consumed, 0 before any.
    fn watermark(&self, scope: &Scope) -> Result<u64, Error>;
    /// Every scope with observations above its watermark: the repository's
    /// scope of each tenant seen, and each person's.
    fn scopes_with_observations(&self) -> Result<Vec<Scope>, Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_keys_round_trip() {
        let repo = Scope::Repo {
            tenant: "acme".into(),
            repo: "app".into(),
        };
        assert_eq!(repo.key(), "repo:acme:app");
        assert_eq!(Scope::parse_key("repo:acme:app").unwrap(), repo);
        let person = Scope::Person {
            tenant: "local".into(),
            subject: "ana".into(),
            repo: Some("app".into()),
        };
        assert_eq!(person.key(), "person:local:ana:app");
        assert_eq!(Scope::parse_key(&person.key()).unwrap(), person);
        let bare = Scope::Person {
            tenant: "local".into(),
            subject: "ana".into(),
            repo: None,
        };
        assert_eq!(Scope::parse_key("person:local:ana").unwrap(), bare);
        assert!(Scope::parse_key("team:x").is_err());
        assert!(Scope::parse_key("repo:acme").is_err());
        assert_eq!(repo.describe(), "the team's memory of app");
        assert_eq!(bare.describe(), "ana's memory");
    }
}
