//! A turn's gateway token, short and renewed while the turn runs.
//!
//! The token a harness can read lives [`super::RENEWED_TTL`] (five minutes), not
//! the turn's hour, so a copy that leaks dies quickly. While the turn runs a
//! [`Renewer`] thread signs a new one before the old expires and replaces
//! the 0600 token file by rename (Anvil's CLI and SDKs read
//! `ANVIL_GATEWAY_TOKEN_FILE` on every call), and keeps the effect proxy's
//! [`LiveToken`] current. The proxy, which sits in front of the gateway,
//! puts the live token on a call whose request carries a token of this turn
//! (any it issued) or none, so a harness that read the file long ago, or
//! cached a token, still reaches the gateway, and a turn runs past
//! [`super::MAX_TTL`]. The thread stops when the turn ends (when
//! [`Renewer`] is dropped), and the token it last signed lapses in minutes.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_support::{best_effort, CondvarExt as _, LockExt as _};

use super::keys::{self, Claims, TokenSigner};

/// How many earlier tokens of the turn the proxy still takes.
const REMEMBERED: usize = 16;

/// The token of the turn now, shared between its [`Renewer`] and the
/// effect proxy.
#[derive(Clone, Debug)]
pub(crate) struct LiveToken(Arc<Mutex<Issued>>);

#[derive(Debug)]
struct Issued {
    current: String,
    earlier: VecDeque<String>,
}

impl LiveToken {
    /// A live token that starts as `token`.
    pub fn new(token: String) -> LiveToken {
        LiveToken(Arc::new(Mutex::new(Issued {
            current: token,
            earlier: VecDeque::new(),
        })))
    }

    /// The token to call the gateway with now.
    pub fn current(&self) -> String {
        self.0.lock_recovering("live token").current.clone()
    }

    /// Whether `presented` is a token this turn issued, now or earlier.
    pub fn issued(&self, presented: &str) -> bool {
        let issued = self.0.lock_recovering("live token");
        std::iter::once(&issued.current)
            .chain(issued.earlier.iter())
            .any(|t| crate::models::gateway::same(presented, t))
    }

    fn replace(&self, token: String) {
        let mut issued = self.0.lock_recovering("live token");
        let old = std::mem::replace(&mut issued.current, token);
        issued.earlier.push_back(old);
        while issued.earlier.len() > REMEMBERED {
            issued.earlier.pop_front();
        }
    }
}

/// What signs the turn's next token: the gateway's signer (its key file or
/// Cloud KMS) and the turn's claims, with a fresh window and `jti` each
/// time.
pub(crate) struct Mint {
    pub keys: Arc<dyn TokenSigner>,
    pub claims: Claims,
    /// How long each token lives.
    pub ttl: Duration,
    /// The turn's deadline in seconds; no token is asked to outlive it by
    /// more than a second.
    pub deadline_s: Option<u64>,
}

impl Mint {
    /// A new token, valid from now.
    pub fn token(&self) -> Result<String, String> {
        let now = branchyard_support::time::now_ms() / 1000;
        let mut exp = now + self.ttl.as_secs().max(1);
        if let Some(deadline) = self.deadline_s {
            exp = exp.min(deadline.max(now + 1));
        }
        let mut claims = self.claims.clone();
        claims.iat = now;
        claims.exp = exp;
        claims.jti = keys::random_id().map_err(|e| e.to_string())?;
        self.keys
            .sign(&claims)
            .map_err(|e| format!("could not sign the gateway token: {e}"))
    }
}

/// Renews a turn's token until dropped.
pub(crate) struct Renewer {
    stop: Arc<(Mutex<bool>, Condvar)>,
    thread: Option<JoinHandle<()>>,
}

impl Renewer {
    /// Sign a new token every half of the mint's TTL, write it to `file`
    /// (0600, by rename) and hand it to `live`.
    pub fn start(mint: Mint, live: LiveToken, file: PathBuf) -> std::io::Result<Renewer> {
        let stop: Arc<(Mutex<bool>, Condvar)> = Arc::default();
        let every = (mint.ttl / 2).max(Duration::from_millis(50));
        let thread = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("by-token-renewal".into())
                .spawn(move || {
                    let (flag, wake) = &*stop;
                    let mut next = Instant::now() + every;
                    loop {
                        let mut stopped = flag.lock_recovering("token renewal");
                        while !*stopped {
                            let left = next.saturating_duration_since(Instant::now());
                            if left.is_zero() {
                                break;
                            }
                            stopped = wake
                                .wait_timeout_recovering(stopped, left, "token renewal")
                                .0;
                        }
                        if *stopped {
                            return;
                        }
                        drop(stopped);
                        next = Instant::now() + every;
                        // A failure keeps the token that stands; the next
                        // round tries again.
                        if let Some(token) = best_effort("renew the gateway token", mint.token()) {
                            // Live first: a token the harness reads from
                            // the file is one the proxy already takes.
                            live.replace(token.clone());
                            best_effort(
                                "write the renewed gateway token",
                                keys::write_private(&file, token.as_bytes()),
                            );
                        }
                    }
                })?
        };
        Ok(Renewer {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for Renewer {
    fn drop(&mut self) {
        {
            let (flag, wake) = &*self.stop;
            *flag.lock_recovering("token renewal") = true;
            wake.notify_all();
        }
        if let Some(thread) = self.thread.take() {
            branchyard_support::join_reporting("token renewal", thread);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connectors::keys::KeyRing;
    use crate::connectors::GrantEntry;
    use branchyard_testkit::wait;

    fn claims() -> Claims {
        Claims {
            iss: "branchyard:local:t".into(),
            aud: "http://127.0.0.1:1/mcp".into(),
            sub: "local:ada".into(),
            iat: 0,
            exp: 0,
            jti: String::new(),
            by_tenant: "local".into(),
            by_branch: "b".into(),
            by_turn: "1".into(),
            by_grants: vec![GrantEntry::parse("github:read").unwrap()],
            by_purpose: None,
            by_models: None,
            by_network: None,
            by_delegation: None,
            by_manifest: Some("blake3:abc".into()),
        }
    }

    fn mint(ttl: Duration) -> Mint {
        Mint {
            keys: Arc::new(KeyRing::generate().unwrap()),
            claims: claims(),
            ttl,
            deadline_s: None,
        }
    }

    #[test]
    fn a_minted_token_has_a_fresh_window_and_id_and_keeps_its_claims() {
        let mint = mint(Duration::from_secs(300));
        let (a, b) = (mint.token().unwrap(), mint.token().unwrap());
        assert_ne!(a, b);
        let jwks = mint.keys.jwks().unwrap();
        let claims = keys::verify(&jwks, &a).unwrap();
        let (iat, exp) = (
            claims["iat"].as_u64().unwrap(),
            claims["exp"].as_u64().unwrap(),
        );
        assert_eq!(exp - iat, 300);
        assert_eq!(claims["by_manifest"], "blake3:abc");
        // Never past the turn's deadline.
        let capped = Mint {
            deadline_s: Some(iat + 10),
            ..mint
        };
        let claims = keys::verify(&jwks, &capped.token().unwrap()).unwrap();
        assert!(claims["exp"].as_u64().unwrap() <= iat + 11, "{claims}");
    }

    #[test]
    fn the_live_token_remembers_what_it_issued_and_nothing_else() {
        let live = LiveToken::new("one".into());
        assert!(live.issued("one") && !live.issued("two"));
        live.replace("two".into());
        assert_eq!(live.current(), "two");
        assert!(live.issued("one") && live.issued("two"));
        for n in 0..(REMEMBERED + 2) {
            live.replace(format!("t{n}"));
        }
        assert!(!live.issued("one"), "the oldest is forgotten");
        assert!(live.issued(&live.current()));
    }

    #[test]
    fn the_renewer_replaces_the_file_and_stops_when_dropped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join(".branchyard/gateway-token");
        let mint = mint(Duration::from_secs(1));
        let first = mint.token().unwrap();
        keys::write_private(&file, first.as_bytes()).unwrap();
        let live = LiveToken::new(first.clone());
        let renewer = Renewer::start(mint, live.clone(), file.clone()).unwrap();
        let second = wait::until("the token file to change", || {
            let now = std::fs::read_to_string(&file).ok()?;
            (now != first).then_some(now)
        });
        assert!(live.issued(&second), "what the file holds is already live");
        assert!(live.issued(&first));
        drop(renewer);
        // Dropping joins the thread: the file and the live token agree, and
        // nothing writes after this.
        let stands = std::fs::read_to_string(&file).unwrap();
        assert_eq!(live.current(), stands);
    }
}
