//! The credential proxy that runs inside a sandbox.
//!
//! The harness is given `http://127.0.0.1:<port>/mcp` as its connector
//! gateway and no token. This proxy listens there, fetches a short token
//! (five minutes) from the Branchyard server as the sandbox's identity,
//! renews it before it expires, and forwards each call to the gateway with
//! the live token. A harness that reads the sandbox's files finds no token
//! file; a copy of a token it caught on the wire dies in minutes.
//!
//! A fetch always proves the turn with its **per-turn secret**, on a header
//! of its own. The engine makes one for every turn whose credentials are
//! held in the sandbox and records only its hash. A guest's proxy gets it in
//! [`ENV_SECRET`]; a sidecar finds it in its settings file in the home the
//! engine copies in for the turn, and that file goes when the turn ends. The
//! manifest digest, the branch and the turn number are not secret, and
//! neither is the service account, so none of them is enough alone.
//!
//! Where the server lists sandbox identities ([`IdentityKind`]), the fetch
//! carries the sandbox's Google ID token too:
//!
//! - **Workload identity.** The sandbox's Google service account (a GKE
//!   Workload Identity service account, or a GCE VM's) signs an ID token for
//!   the server's audience, which the proxy reads from the metadata server.
//!   The server holds an allowlist of those accounts, and requires one *in
//!   addition to* the secret, so a secret that leaked from a sandbox is of
//!   no use from anywhere else.
//!
//! The fetch names the turn (`manifest`, `branch`, `turn`) and the server
//! issues a token only for that turn while it runs, with the claims of the
//! turn's recorded grant ([`super::issue`]).

use std::io::{self, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use branchyard_support::{best_effort, CondvarExt as _, LockExt as _};
use serde::{Deserialize, Serialize};

use super::issue::{TokenRequest, TokenResponse, SECRET_HEADER, TOKEN_PATH};
use crate::effects::proxy::respond;
use crate::models::gateway::{read_request, Request};
use crate::models::upstream::{self, Failure, Target, Transport};

/// The port a sandbox's proxy listens on, on loopback, when none is chosen.
pub const DEFAULT_PORT: u16 = 18900;

/// The variable that hands the proxy its per-turn secret.
pub const ENV_SECRET: &str = "BRANCHYARD_CREDENTIAL_SECRET";

/// The standard variable that names a metadata server, `host[:port]`.
pub const ENV_METADATA_HOST: &str = "GCE_METADATA_HOST";

/// The metadata server a Google VM or GKE pod reaches.
const METADATA_HOST: &str = "metadata.google.internal";

/// What the first line a ready proxy prints starts with, then its address.
pub const READY: &str = "listening";

/// The longest answer from the server or the metadata server.
const MAX_ANSWER: u64 = 64 * 1024;

/// The longest answer from the gateway the proxy relays.
const MAX_RELAYED: u64 = 64 * 1024 * 1024;

/// How long one call to the server or the metadata server may take.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);

/// How soon a failed fetch is tried again.
const RETRY: Duration = Duration::from_secs(1);

/// What a sandbox proves to the server besides the turn's secret.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IdentityKind {
    /// Also a Google ID token for `audience`, from the metadata server.
    Workload {
        /// The `aud` the server verifies (`connectors.sandbox_audience`).
        audience: String,
    },
    /// The per-turn secret alone.
    Secret,
}

/// Everything a sandbox's proxy needs to run, as a file or command line
/// gives it. A guest's proxy gets its secret in [`ENV_SECRET`]; the file a
/// sidecar reads holds it in `secret`, which is why this type's `Debug`
/// does not print it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyConfig {
    /// The address to listen on, such as `127.0.0.1:18900`; port 0 picks one.
    pub listen: String,
    /// The Branchyard server's URL as the sandbox reaches it.
    pub server: String,
    /// The connector gateway's `/mcp` URL as the sandbox reaches it.
    pub upstream: String,
    /// The turn manifest's digest the tokens are bound to.
    pub manifest: String,
    /// The branch as the server names it in tokens: `<repo>/<branch>`.
    pub branch: String,
    /// The turn's number.
    pub turn: u32,
    /// What the sandbox proves besides the secret.
    pub identity: IdentityKind,
    /// The turn's secret, for a proxy that reads it from its settings file
    /// (a sidecar). Absent when the secret is given in [`ENV_SECRET`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret: Option<String>,
}

impl std::fmt::Debug for ProxyConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProxyConfig")
            .field("listen", &self.listen)
            .field("server", &self.server)
            .field("upstream", &self.upstream)
            .field("manifest", &self.manifest)
            .field("branch", &self.branch)
            .field("turn", &self.turn)
            .field("identity", &self.identity)
            .field("secret", &self.secret.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

impl ProxyConfig {
    /// The URL a harness in the same sandbox is given as its gateway, for a
    /// proxy that listens on `port`.
    pub fn harness_url(port: u16) -> String {
        format!("http://127.0.0.1:{port}/mcp")
    }
}

/// A token and when it lapses.
#[derive(Clone, Debug)]
struct Fetched {
    token: String,
    /// When it was fetched, milliseconds since the epoch.
    at_ms: u64,
    /// When it expires, milliseconds since the epoch.
    expires_ms: u64,
}

impl Fetched {
    /// When half of its life has passed: the time to fetch the next.
    fn half_ms(&self) -> u64 {
        self.at_ms + (self.expires_ms.saturating_sub(self.at_ms)) / 2
    }

    /// Whether it is good for a call now, with a margin of a tenth of its
    /// life (at most five seconds) before it lapses.
    fn fresh(&self, now_ms: u64) -> bool {
        let life = self.expires_ms.saturating_sub(self.at_ms);
        now_ms + (life / 10).min(5_000) < self.expires_ms
    }
}

/// What proves the sandbox, resolved: the turn's secret, and the workload
/// identity beside it where the server lists identities.
struct Proof {
    secret: String,
    workload: Option<(String, Target)>,
}

/// Asks the server for the turn's token.
struct Fetcher {
    config: ProxyConfig,
    server: Target,
    proof: Proof,
    transport: Transport,
}

impl Fetcher {
    fn new(
        config: &ProxyConfig,
        secret: Option<String>,
        metadata_host: Option<String>,
    ) -> Result<Fetcher, String> {
        let server = Target::parse(&config.server)
            .map_err(|e| format!("the server URL {:?} is not usable: {e}", config.server))?;
        let secret = secret
            .or_else(|| config.secret.clone())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                format!("the per-turn secret is in neither {ENV_SECRET} nor the settings file")
            })?;
        let workload = match &config.identity {
            IdentityKind::Secret => None,
            IdentityKind::Workload { audience } => {
                let host = metadata_host
                    .filter(|h| !h.is_empty())
                    .unwrap_or_else(|| METADATA_HOST.to_owned());
                let metadata = Target::parse(&format!("http://{host}"))
                    .map_err(|e| format!("the metadata host {host:?} is not usable: {e}"))?;
                Some((audience.clone(), metadata))
            }
        };
        let proof = Proof { secret, workload };
        Ok(Fetcher {
            config: config.clone(),
            server,
            proof,
            transport: Transport::from_env(FETCH_TIMEOUT),
        })
    }

    /// Google's ID token for this sandbox's service account.
    fn identity_token(&self, audience: &str, metadata: &Target) -> Result<String, String> {
        let path = format!(
            "/computeMetadata/v1/instance/service-accounts/default/identity?audience={}&format=full",
            percent_encoding::utf8_percent_encode(audience, percent_encoding::NON_ALPHANUMERIC)
        );
        let headers = [("Metadata-Flavor".to_owned(), "Google".to_owned())];
        let mut response =
            upstream::send_within(metadata, "GET", &path, &headers, &[], FETCH_TIMEOUT)
                .map_err(|e| format!("the metadata server gave no identity token: {e}"))?;
        let text = read_text(&mut response.body)?;
        if response.status != 200 {
            return Err(format!(
                "the metadata server refused an identity token ({}): {}",
                response.status,
                text.trim()
            ));
        }
        Ok(text.trim().to_owned())
    }

    fn fetch(&self) -> Result<Fetched, String> {
        let mut headers = vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("Accept".to_owned(), "application/json".to_owned()),
        ];
        headers.push((SECRET_HEADER.to_owned(), self.proof.secret.clone()));
        if let Some((audience, metadata)) = &self.proof.workload {
            let id = self.identity_token(audience, metadata)?;
            headers.push(("Authorization".to_owned(), format!("Bearer {id}")));
        }
        let body = serde_json::to_vec(&TokenRequest {
            manifest: self.config.manifest.clone(),
            branch: self.config.branch.clone(),
            turn: self.config.turn,
        })
        .map_err(|e| e.to_string())?;
        let at_ms = branchyard_support::time::now_ms();
        let mut response = self
            .transport
            .send(&self.server, "POST", TOKEN_PATH, &headers, &body)
            .map_err(|e| format!("the server gave no token: {e}"))?;
        let text = read_text(&mut response.body)?;
        if response.status != 200 {
            return Err(format!(
                "the server refused a token ({}): {}",
                response.status,
                refusal(&text)
            ));
        }
        let issued: TokenResponse = serde_json::from_str(&text)
            .map_err(|e| format!("the server's token answer is not understood: {e}"))?;
        Ok(Fetched {
            token: issued.token,
            at_ms,
            expires_ms: issued.expires_at.saturating_mul(1000),
        })
    }
}

/// The code and message of an error body, else the body.
fn refusal(text: &str) -> String {
    serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| {
            let error = v.get("error")?;
            Some(format!(
                "{}: {}",
                error.get("code")?.as_str()?,
                error.get("message")?.as_str()?
            ))
        })
        .unwrap_or_else(|| text.trim().to_owned())
}

fn read_text(body: &mut impl Read) -> Result<String, String> {
    let mut bytes = Vec::new();
    body.take(MAX_ANSWER)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// The turn's live token: fetched, and renewed at half its life.
struct Live {
    fetcher: Fetcher,
    current: Mutex<Option<Fetched>>,
    /// Serializes fetches, so a call and the renewer do not both ask.
    fetching: Mutex<()>,
    /// Why the last fetch failed, until one succeeds.
    failure: Mutex<Option<String>>,
    stop: (Mutex<bool>, Condvar),
}

impl Live {
    /// Fetch now and keep the result.
    fn refresh(&self) -> Result<Fetched, String> {
        let _one_at_a_time = self.fetching.lock_recovering("credential fetch");
        let result = self.fetcher.fetch();
        match &result {
            Ok(fetched) => {
                *self.current.lock_recovering("live credential") = Some(fetched.clone());
                *self.failure.lock_recovering("credential failure") = None;
            }
            Err(why) => *self.failure.lock_recovering("credential failure") = Some(why.clone()),
        }
        result
    }

    /// The token to call the gateway with now: the held one while it is
    /// good, else a fresh one; and when a fetch fails, the held one for as
    /// long as it has not lapsed.
    fn token(&self) -> Result<String, String> {
        let held = self.current.lock_recovering("live credential").clone();
        let now = branchyard_support::time::now_ms();
        match held {
            Some(f) if f.fresh(now) => Ok(f.token),
            Some(f) => match self.refresh() {
                Ok(fresh) => Ok(fresh.token),
                Err(_) if f.expires_ms > branchyard_support::time::now_ms() => Ok(f.token),
                Err(why) => Err(why),
            },
            None => self.refresh().map(|f| f.token),
        }
    }

    /// How long until the renewer's next fetch: the half of the held
    /// token's life, else a retry's pause (also after a fetch that failed
    /// once the half has passed).
    fn next(&self) -> Duration {
        let held = self.current.lock_recovering("live credential").clone();
        let left = held.map_or(0, |f| {
            f.half_ms()
                .saturating_sub(branchyard_support::time::now_ms())
        });
        match left {
            0 => RETRY,
            ms => Duration::from_millis(ms),
        }
    }

    /// Renew until stopped.
    fn renew(&self) {
        let (flag, wake) = &self.stop;
        loop {
            let until = Instant::now() + self.next();
            let mut stopped = flag.lock_recovering("credential renewal");
            while !*stopped {
                let left = until.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                stopped = wake
                    .wait_timeout_recovering(stopped, left, "credential renewal")
                    .0;
            }
            if *stopped {
                return;
            }
            drop(stopped);
            // A failure keeps the token that stands (until it lapses); the
            // next round tries again.
            best_effort("renew the turn's gateway token", self.refresh());
        }
    }

    fn stop(&self) {
        let (flag, wake) = &self.stop;
        *flag.lock_recovering("credential renewal") = true;
        wake.notify_all();
    }
}

/// A running proxy; stopped when dropped.
pub struct CredentialProxy {
    live: Arc<Live>,
    address: SocketAddr,
    stopping: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
    renew: Option<JoinHandle<()>>,
}

impl CredentialProxy {
    /// Bind `config.listen` and serve, with the first token fetched first
    /// when `first` is set: its failure is this call's, so a sandbox that
    /// cannot get a token fails to start the harness rather than the harness's
    /// first call.
    pub fn start(
        config: &ProxyConfig,
        secret: Option<String>,
        first: bool,
    ) -> Result<CredentialProxy, String> {
        CredentialProxy::start_at(config, secret, first, std::env::var(ENV_METADATA_HOST).ok())
    }

    /// [`CredentialProxy::start`] with the metadata server's `host[:port]`
    /// given (`None`: Google's).
    pub fn start_at(
        config: &ProxyConfig,
        secret: Option<String>,
        first: bool,
        metadata_host: Option<String>,
    ) -> Result<CredentialProxy, String> {
        let upstream = Target::parse(&config.upstream)
            .map_err(|e| format!("the gateway URL {:?} is not usable: {e}", config.upstream))?;
        let fetcher = Fetcher::new(config, secret, metadata_host)?;
        let listener = TcpListener::bind(&config.listen)
            .map_err(|e| format!("could not listen on {}: {e}", config.listen))?;
        let address = listener.local_addr().map_err(|e| e.to_string())?;
        let live = Arc::new(Live {
            fetcher,
            current: Mutex::new(None),
            fetching: Mutex::new(()),
            failure: Mutex::new(None),
            stop: (Mutex::new(false), Condvar::new()),
        });
        if first {
            live.refresh()?;
        }
        let stopping = Arc::new(AtomicBool::new(false));
        let renew = {
            let live = live.clone();
            std::thread::Builder::new()
                .name("by-credential-renewal".into())
                .spawn(move || live.renew())
                .map_err(|e| e.to_string())?
        };
        let accept = {
            let (live, stopping) = (live.clone(), stopping.clone());
            std::thread::Builder::new()
                .name("by-credential-proxy".into())
                .spawn(move || accept(listener, &live, &upstream, &stopping))
                .map_err(|e| e.to_string())?
        };
        Ok(CredentialProxy {
            live,
            address,
            stopping,
            accept: Some(accept),
            renew: Some(renew),
        })
    }

    /// The address it listens on.
    pub fn address(&self) -> SocketAddr {
        self.address
    }
}

impl Drop for CredentialProxy {
    fn drop(&mut self) {
        self.live.stop();
        if !self.stopping.swap(true, Ordering::SeqCst) {
            // Wake the accept loop so it sees the flag.
            best_effort(
                "connect to wake the accept loop",
                TcpStream::connect_timeout(&self.address, Duration::from_secs(1)),
            );
        }
        for thread in [self.accept.take(), self.renew.take()]
            .into_iter()
            .flatten()
        {
            branchyard_support::join_reporting("credential proxy", thread);
        }
    }
}

fn accept(listener: TcpListener, live: &Arc<Live>, upstream: &Target, stopping: &AtomicBool) {
    for stream in listener.incoming() {
        if stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let (live, upstream) = (live.clone(), upstream.clone());
        let spawned = std::thread::Builder::new()
            .name("by-credential-call".into())
            .spawn(move || handle(stream, &live, &upstream));
        if spawned.is_err() {
            return;
        }
    }
}

/// Whether the harness's header `name` goes to the gateway: not the
/// hop-by-hop and framing headers the effect proxy also drops, and not a
/// credential, which the live token replaces. The harness's own
/// `Idempotency-Key` goes through (the effect proxy sets its own).
fn forwarded(name: &str) -> bool {
    let own = name.eq_ignore_ascii_case("idempotency-key");
    let credential =
        name.eq_ignore_ascii_case("authorization") || name.eq_ignore_ascii_case("x-api-key");
    (own || !crate::effects::proxy::dropped(name)) && !credential
}

/// Answer without forwarding: a JSON error.
fn refuse(out: &mut TcpStream, status: u16, code: &str, message: &str) {
    let body = serde_json::json!({"error": {"code": code, "message": message}}).to_string();
    respond(
        out,
        status,
        &[("Content-Type".into(), "application/json".into())],
        body.as_bytes(),
    );
}

/// Forward one call with the live token, and answer with what the gateway
/// said, read whole (as the effect proxy does).
fn handle(stream: TcpStream, live: &Live, upstream: &Target) {
    let Ok(mut out) = stream.try_clone() else {
        return;
    };
    best_effort(
        "set the call's read timeout",
        stream.set_read_timeout(Some(Duration::from_secs(300))),
    );
    let mut reader = BufReader::new(stream);
    let request: Request = match read_request(&mut reader, &mut out) {
        Ok(Some(request)) => request,
        Ok(None) => return,
        Err(why) => {
            refuse(&mut out, 400, "bad_request", &why);
            return;
        }
    };
    let token = match live.token() {
        Ok(token) => token,
        Err(why) => {
            refuse(
                &mut out,
                502,
                "credential_unavailable",
                &format!("Branchyard's credential proxy has no gateway token: {why}"),
            );
            return;
        }
    };
    // `/mcp...` maps onto the gateway's URL; anything else goes to the
    // gateway's host as it is.
    let root = Target {
        prefix: String::new(),
        ..upstream.clone()
    };
    let path = match request.target.strip_prefix("/mcp") {
        Some(rest) if rest.is_empty() || rest.starts_with(['/', '?']) => {
            format!("{}{rest}", upstream.prefix)
        }
        _ => request.target.clone(),
    };
    let mut headers: Vec<(String, String)> = request
        .headers
        .iter()
        .filter(|(n, _)| forwarded(n))
        .cloned()
        .collect();
    headers.push(("Accept-Encoding".into(), "identity".into()));
    headers.push(("Authorization".into(), format!("Bearer {token}")));
    let mut response = match upstream::send(&root, &request.method, &path, &headers, &request.body)
    {
        Ok(response) => response,
        Err(Failure::Connect(why) | Failure::Exchange(why)) => {
            refuse(
                &mut out,
                502,
                "gateway_unreachable",
                &format!("Branchyard's credential proxy could not reach the gateway: {why}"),
            );
            return;
        }
    };
    let mut body = Vec::new();
    if let Err(why) = (&mut response.body)
        .take(MAX_RELAYED)
        .read_to_end(&mut body)
    {
        refuse(
            &mut out,
            502,
            "gateway_unreachable",
            &format!("Branchyard's credential proxy lost the gateway mid-answer: {why}"),
        );
        return;
    }
    respond(&mut out, response.status, &response.headers, &body);
}

/// Run the proxy for `config` until the process ends: print
/// `listening <address>` once it is ready, then serve. `secret` is
/// [`ENV_SECRET`]'s value. With `first`, a failure to fetch the first token
/// is this call's error.
pub fn run(config: &ProxyConfig, secret: Option<String>, first: bool) -> Result<(), String> {
    let proxy = CredentialProxy::start(config, secret, first)?;
    let mut stdout = io::stdout();
    writeln!(stdout, "{READY} {}", proxy.address())
        .and_then(|()| stdout.flush())
        .map_err(|e| format!("could not say it is ready: {e}"))?;
    loop {
        std::thread::park();
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)] // tests: a panic is the failure report
#[cfg(test)]
mod tests;
