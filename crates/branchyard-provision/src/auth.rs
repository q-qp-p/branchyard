// Derived from Scion (https://github.com/GoogleCloudPlatform/scion) at
// d9b9e6a2e1e29e428e6f8e72c2d5ab0df0475338: AuthMethod, AuthSpec,
// ResolvedAuth and ProvisionContext.select_auth of
// harnesses/scion_harness.py, and its tests in
// harnesses/scion_harness_test.py. Copyright 2026 Google LLC. Licensed under
// the Apache License, Version 2.0.
//
// Modified for Branchyard: translated from Python to Rust over the names of
// the secrets given, with no candidates file, no environment fallback and
// no check for a credential file already on disk (planning does no I/O);
// no secrets means no authentication is provisioned rather than Scion's
// no-auth gate; secrets none of whose names a harness reads select nothing
// and are reported as unused; an `all_of` entry is a group of names, one of
// which is needed; and a method given some of the secrets it needs but not
// all selects nothing ([`Spec::incomplete`] says what it lacks) unless it was
// asked for, where Scion refuses.

//! Choosing how a harness authenticates from the secrets it was given.

use crate::Refused;

/// How one authentication method is recognized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// One secret of every `all_of` group and the first present `any_of`
    /// secret. Within a group, as in `any_of`, the first present one counts.
    Env {
        any_of: &'static [&'static str],
        all_of: &'static [&'static [&'static str]],
    },
    /// A file's whole content, given as the secret `secret`.
    File { secret: &'static str },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Method {
    pub name: &'static str,
    pub kind: Kind,
    /// What to provide, for refusals.
    pub hint: &'static str,
}

impl Method {
    pub const fn env(
        name: &'static str,
        any_of: &'static [&'static str],
        hint: &'static str,
    ) -> Method {
        Method {
            name,
            kind: Kind::Env {
                any_of,
                all_of: &[],
            },
            hint,
        }
    }

    pub const fn env_all(
        name: &'static str,
        all_of: &'static [&'static [&'static str]],
        any_of: &'static [&'static str],
        hint: &'static str,
    ) -> Method {
        Method {
            name,
            kind: Kind::Env { any_of, all_of },
            hint,
        }
    }

    pub const fn file(name: &'static str, secret: &'static str, hint: &'static str) -> Method {
        Method {
            name,
            kind: Kind::File { secret },
            hint,
        }
    }

    /// Every secret name this method reads.
    pub fn names(&self) -> Vec<&'static str> {
        match self.kind {
            Kind::Env { any_of, all_of } => all_of
                .iter()
                .flat_map(|group| group.iter())
                .chain(any_of)
                .copied()
                .collect(),
            Kind::File { secret } => vec![secret],
        }
    }

    /// For an environment method given some of the secrets it reads but
    /// not all it needs, what it lacks: each missing group as `A, B or C`,
    /// joined by `and`. `None` when it has all it needs or was given none.
    fn missing(&self, given: &[&str]) -> Option<String> {
        let Kind::Env { any_of, all_of } = self.kind else {
            return None;
        };
        if !self.names().iter().any(|name| given.contains(name)) {
            return None;
        }
        let groups = all_of
            .iter()
            .copied()
            .chain(Some(any_of).filter(|a| !a.is_empty()));
        let missing: Vec<String> = groups
            .filter(|group| first_given(group, given).is_none())
            .map(|group| match group.split_last() {
                Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
                _ => group.concat(),
            })
            .collect();
        (!missing.is_empty()).then(|| missing.join(" and "))
    }
}

/// The first of `names` that `given` holds.
fn first_given(names: &[&'static str], given: &[&str]) -> Option<&'static str> {
    names.iter().copied().find(|name| given.contains(name))
}

/// A method given some of the secrets it reads and not all it needs, which
/// therefore selects nothing: Claude Code's `vertex-ai` with a project and
/// no region, say.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Incomplete {
    /// The method's name: `vertex-ai`.
    pub method: &'static str,
    /// The secrets of it that were given.
    pub given: Vec<&'static str>,
    /// What it lacks: `CLOUD_ML_REGION, GOOGLE_CLOUD_LOCATION or
    /// GOOGLE_CLOUD_REGION`.
    pub missing: String,
}

/// A harness's methods, in order of precedence.
#[derive(Clone, Copy, Debug)]
pub struct Spec {
    pub harness: &'static str,
    pub methods: &'static [Method],
}

/// The chosen method.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub method: &'static str,
    /// For an environment method, the `any_of` secret chosen, or the first
    /// `all_of` one when it has no `any_of`.
    pub env_key: Option<&'static str>,
    /// For a file method, the secret holding the file.
    pub file_secret: Option<&'static str>,
}

impl Resolved {
    /// The secret an environment method reads. A method without one is
    /// refused: the specs give every environment method a key.
    pub fn env(&self) -> Result<&'static str, Refused> {
        self.env_key.ok_or_else(|| {
            Refused(format!(
                "auth method {:?} names no environment key",
                self.method
            ))
        })
    }

    /// The secret the method delivers, whether it is read from the
    /// environment or from a file.
    pub fn secret(&self) -> Result<&'static str, Refused> {
        self.env_key
            .or(self.file_secret)
            .ok_or_else(|| Refused(format!("auth method {:?} names no secret", self.method)))
    }
}

impl Spec {
    /// Every secret name any method reads.
    pub fn names(&self) -> Vec<&'static str> {
        let mut names: Vec<_> = self.methods.iter().flat_map(Method::names).collect();
        names.dedup();
        names
    }

    fn valid(&self) -> String {
        self.methods
            .iter()
            .map(|m| m.name)
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Choose a method from the secrets `given` names: the explicit one if
    /// asked for, else the first that matches. `None` when none matches,
    /// which without an explicit method is never a refusal: a method given
    /// only part of what it needs is [`Spec::incomplete`].
    pub fn select(
        &self,
        given: &[&str],
        explicit: Option<&str>,
    ) -> Result<Option<Resolved>, Refused> {
        let has = |name: &str| given.contains(&name);
        if let Some(explicit) = explicit {
            if !self.methods.iter().any(|m| m.name == explicit) {
                return Err(Refused(format!(
                    "{}: unknown auth type {explicit:?}; valid types are: {}",
                    self.harness,
                    self.valid()
                )));
            }
        }
        for method in self.methods {
            if explicit.is_some_and(|e| e != method.name) {
                continue;
            }
            let matched = match method.kind {
                Kind::Env { any_of, all_of } => {
                    if all_of
                        .iter()
                        .all(|group| first_given(group, given).is_some())
                    {
                        match (any_of, all_of) {
                            ([], [first, ..]) => first_given(first, given).map(Some),
                            ([], []) => None,
                            (any, _) => first_given(any, given).map(Some),
                        }
                    } else {
                        None
                    }
                }
                Kind::File { secret } => has(secret).then_some(None),
            };
            match (matched, method.kind) {
                (Some(env_key), Kind::Env { .. }) => {
                    return Ok(Some(Resolved {
                        method: method.name,
                        env_key,
                        file_secret: None,
                    }))
                }
                (Some(_), Kind::File { secret }) => {
                    return Ok(Some(Resolved {
                        method: method.name,
                        env_key: None,
                        file_secret: Some(secret),
                    }))
                }
                (None, _) if explicit.is_some() => {
                    let why = match method.missing(given) {
                        Some(missing) => format!("also needs {missing}"),
                        None => "selected but no credentials found".into(),
                    };
                    return Err(Refused(format!(
                        "{}: auth type {:?} {why}; {}",
                        self.harness, method.name, method.hint
                    )));
                }
                (None, _) => {}
            }
        }
        Ok(None)
    }

    /// The methods `given` holds part of what they need, and what each
    /// lacks.
    pub fn incomplete(&self, given: &[&str]) -> Vec<Incomplete> {
        self.methods
            .iter()
            .filter_map(|method| {
                Some(Incomplete {
                    method: method.name,
                    missing: method.missing(given)?,
                    given: method
                        .names()
                        .into_iter()
                        .filter(|name| given.contains(name))
                        .collect(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // From Scion's scion_harness_test.py.

    const EXPLICIT: Spec = Spec {
        harness: "test",
        methods: &[
            Method::env("api-key", &["API_KEY"], "set API_KEY"),
            Method::env("oauth-token", &["OAUTH_TOKEN"], "set OAUTH_TOKEN"),
            Method::file("auth-file", "AUTH_FILE", "provide AUTH_FILE"),
        ],
    };

    #[test]
    fn explicit_valid_type_present() {
        let got = EXPLICIT
            .select(&["API_KEY"], Some("api-key"))
            .unwrap()
            .unwrap();
        assert_eq!((got.method, got.env_key), ("api-key", Some("API_KEY")));
    }

    #[test]
    fn explicit_invalid_type_raises() {
        let error = EXPLICIT.select(&[], Some("magic")).unwrap_err().0;
        assert!(
            error.contains("magic") && error.contains("valid types"),
            "{error}"
        );
    }

    #[test]
    fn explicit_type_missing_creds_raises() {
        let error = EXPLICIT.select(&[], Some("api-key")).unwrap_err().0;
        assert!(error.contains("api-key"), "{error}");
    }

    const TWO: Spec = Spec {
        harness: "test",
        methods: &[
            Method::env("primary", &["PRIMARY_KEY"], ""),
            Method::env("secondary", &["SECONDARY_KEY"], ""),
        ],
    };

    #[test]
    fn first_match_wins() {
        let got = TWO
            .select(&["SECONDARY_KEY", "PRIMARY_KEY"], None)
            .unwrap()
            .unwrap();
        assert_eq!((got.method, got.env_key), ("primary", Some("PRIMARY_KEY")));
    }

    #[test]
    fn fallback_to_second() {
        let got = TWO.select(&["SECONDARY_KEY"], None).unwrap().unwrap();
        assert_eq!(got.method, "secondary");
    }

    #[test]
    fn any_of_picks_first_present() {
        const SPEC: Spec = Spec {
            harness: "test",
            methods: &[Method::env("api-key", &["KEY_A", "KEY_B", "KEY_C"], "")],
        };
        let got = SPEC.select(&["KEY_C", "KEY_B"], None).unwrap().unwrap();
        assert_eq!(got.env_key, Some("KEY_B"));
    }

    #[test]
    fn no_candidates_selects_nothing() {
        // Scion's no-auth gate: Branchyard provisions no authentication.
        assert_eq!(TWO.select(&[], None).unwrap(), None);
        assert_eq!(TWO.select(&["UNRELATED"], None).unwrap(), None);
    }

    const VERTEX: Spec = Spec {
        harness: "test",
        methods: &[Method::env_all(
            "vertex-ai",
            &[&["GOOGLE_CLOUD_PROJECT"]],
            &["GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"],
            "provide GOOGLE_CLOUD_PROJECT and GOOGLE_CLOUD_LOCATION",
        )],
    };

    #[test]
    fn all_of_all_present() {
        let got = VERTEX
            .select(&["GOOGLE_CLOUD_PROJECT", "GOOGLE_CLOUD_LOCATION"], None)
            .unwrap()
            .unwrap();
        assert_eq!(
            (got.method, got.env_key),
            ("vertex-ai", Some("GOOGLE_CLOUD_LOCATION"))
        );
    }

    /// Scion refuses a partial set; here it selects nothing, and says what
    /// is missing.
    #[test]
    fn all_of_missing_one() {
        assert_eq!(
            VERTEX.select(&["GOOGLE_CLOUD_LOCATION"], None).unwrap(),
            None
        );
        assert_eq!(
            VERTEX.incomplete(&["GOOGLE_CLOUD_LOCATION", "UNRELATED"]),
            [Incomplete {
                method: "vertex-ai",
                given: vec!["GOOGLE_CLOUD_LOCATION"],
                missing: "GOOGLE_CLOUD_PROJECT".into(),
            }]
        );
    }

    /// A group is one of several names, the first given one counting; a
    /// method missing a group is incomplete, and refused only when asked
    /// for, naming what it lacks.
    #[test]
    fn a_group_takes_any_of_its_names_and_names_them_all_when_missing() {
        const SPEC: Spec = Spec {
            harness: "test",
            methods: &[
                Method::env("api-key", &["API_KEY"], "set API_KEY"),
                Method::env_all(
                    "vertex-ai",
                    &[&["OWN_PROJECT", "GOOGLE_CLOUD_PROJECT"]],
                    &["OWN_REGION", "GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"],
                    "set a project and a region",
                ),
            ],
        };
        let got = SPEC
            .select(
                &["GOOGLE_CLOUD_PROJECT", "OWN_PROJECT", "GOOGLE_CLOUD_REGION"],
                None,
            )
            .unwrap()
            .unwrap();
        assert_eq!(
            (got.method, got.env_key),
            ("vertex-ai", Some("GOOGLE_CLOUD_REGION"))
        );
        let partial = ["GOOGLE_CLOUD_PROJECT"];
        assert_eq!(SPEC.select(&partial, None).unwrap(), None);
        let gaps = SPEC.incomplete(&partial);
        assert_eq!(gaps.len(), 1);
        assert_eq!(
            (
                gaps[0].method,
                gaps[0].given.as_slice(),
                gaps[0].missing.as_str()
            ),
            (
                "vertex-ai",
                &["GOOGLE_CLOUD_PROJECT"][..],
                "OWN_REGION, GOOGLE_CLOUD_LOCATION or GOOGLE_CLOUD_REGION"
            )
        );
        let error = SPEC.select(&partial, Some("vertex-ai")).unwrap_err().0;
        assert_eq!(
            error,
            "test: auth type \"vertex-ai\" also needs OWN_REGION, GOOGLE_CLOUD_LOCATION or \
             GOOGLE_CLOUD_REGION; set a project and a region"
        );
        // Beside a method that has all it needs, the partial one is passed.
        let got = SPEC
            .select(&["OWN_REGION", "API_KEY"], None)
            .unwrap()
            .unwrap();
        assert_eq!(got.method, "api-key");
        let error = SPEC.select(&[], Some("vertex-ai")).unwrap_err().0;
        assert!(
            error.contains("selected but no credentials found"),
            "{error}"
        );
    }

    const CLAUDE_LIKE: Spec = Spec {
        harness: "claude",
        methods: &[
            Method::env("api-key", &["ANTHROPIC_API_KEY"], ""),
            Method::env("oauth-token", &["CLAUDE_CODE_OAUTH_TOKEN"], ""),
            Method::env_all(
                "vertex-ai",
                &[&["GOOGLE_CLOUD_PROJECT"]],
                &["GOOGLE_CLOUD_LOCATION", "GOOGLE_CLOUD_REGION"],
                "",
            ),
        ],
    };

    #[test]
    fn explicit_vertex_ai_wins_over_api_key() {
        let given = [
            "ANTHROPIC_API_KEY",
            "GOOGLE_CLOUD_PROJECT",
            "GOOGLE_CLOUD_REGION",
        ];
        let got = CLAUDE_LIKE
            .select(&given, Some("vertex-ai"))
            .unwrap()
            .unwrap();
        assert_eq!(got.method, "vertex-ai");
        let got = CLAUDE_LIKE.select(&given, None).unwrap().unwrap();
        assert_eq!(got.method, "api-key");
    }

    #[test]
    fn file_methods_match_their_secret() {
        let got = EXPLICIT.select(&["AUTH_FILE"], None).unwrap().unwrap();
        assert_eq!(
            (got.method, got.file_secret),
            ("auth-file", Some("AUTH_FILE"))
        );
    }
}
