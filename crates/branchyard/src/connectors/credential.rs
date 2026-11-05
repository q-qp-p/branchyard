//! Where a turn's gateway credentials are held, and who decides.
//!
//! A harness should hold no gateway token. Where the provider lets it, the
//! credential proxy ([`super::sandbox_proxy`]) runs inside the sandbox and
//! fetches the turn's short token from the server. [`CredentialSite`] is the
//! provider's side of that choice (what it *can* run, declared once per
//! provider in `providers/`), [`CredentialProxy`] is the operator's (the
//! server URL a sandbox reaches, which switches it on), and `choose` joins
//! them. Everything else keeps the proxy on the host that runs the turn,
//! which renews the token and replaces the token file (`renewal.rs`).
//!
//! | provider | site | where the proxy runs |
//! |---|---|---|
//! | local process | [`CredentialSite::Host`] | on the host |
//! | container, recipe machine | [`CredentialSite::Host`] | on the host |
//! | Microsandbox | [`CredentialSite::Guest`] | a process the engine starts in the guest, with the per-turn secret |
//! | GKE Agent Sandbox | [`CredentialSite::Sidecar`] | a container beside the harness in the pod, as the pod's Workload Identity |
//! | Substrate | [`CredentialSite::Sidecar`] | a process the actor template runs, as the actor's identity |

use super::sandbox_proxy::{IdentityKind, ProxyConfig, DEFAULT_PORT};

/// Where a provider can run a turn's credential proxy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialSite {
    /// Only on the host that runs the turn: the provider has no place in
    /// the sandbox to start one (or no sandbox).
    Host,
    /// A process the engine starts in the sandbox before the harness, on
    /// the sandbox's loopback. It proves the turn with a per-turn secret.
    Guest,
    /// A container or process the sandbox's own definition runs beside the
    /// harness, on the same loopback, started by the sandbox and not by the
    /// engine. It proves the turn with the sandbox's workload identity, and
    /// finds its settings in a file in the home.
    Sidecar,
}

/// The operator's settings for sandboxed credential proxies
/// (`[connectors] sandbox_server` on a server).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CredentialProxy {
    /// This server's URL as a sandbox reaches it. Without it, every turn
    /// keeps the host's proxy.
    pub server: Option<String>,
    /// The audience a sandbox's Google identity token is for
    /// (`connectors.sandbox_audience`, default the issuer).
    pub audience: String,
    /// The loopback port the proxy listens on in a sandbox.
    pub port: u16,
    /// The `by` a Microsandbox guest runs (on its `PATH` or an absolute
    /// guest path).
    pub guest_command: String,
    /// The image of a GKE Agent Sandbox's sidecar container
    /// (`repository@sha256:digest`, with `by` on its `PATH`); the
    /// allocation's own when unset.
    pub sidecar_image: Option<String>,
}

impl Default for CredentialProxy {
    fn default() -> CredentialProxy {
        CredentialProxy {
            server: None,
            audience: String::new(),
            port: DEFAULT_PORT,
            guest_command: "by".to_owned(),
            sidecar_image: None,
        }
    }
}

/// What a turn's credentials do, once the provider and the operator have
/// both been asked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// The host's proxy holds the token, renews it, and writes the file the
    /// harness reads.
    Host,
    /// The sandbox's proxy does, as `config` says.
    Sandbox {
        /// Where it runs.
        site: CredentialSite,
        /// What it is started with.
        config: ProxyConfig,
    },
}

/// Who holds the credentials of a turn run by a provider whose site is
/// `site`, for a turn named by `manifest`, `branch` and `turn`, whose
/// gateway is `upstream` as the sandbox reaches it.
pub(crate) fn choose(
    site: CredentialSite,
    proxy: &CredentialProxy,
    upstream: &str,
    turn: (&str, &str, u32),
) -> Holder {
    let (Some(server), true) = (&proxy.server, site != CredentialSite::Host) else {
        return Holder::Host;
    };
    let (manifest, branch, number) = turn;
    Holder::Sandbox {
        site,
        config: ProxyConfig {
            listen: format!("127.0.0.1:{}", proxy.port),
            server: server.clone(),
            upstream: upstream.to_owned(),
            manifest: manifest.to_owned(),
            branch: branch.to_owned(),
            turn: number,
            identity: match site {
                CredentialSite::Sidecar => IdentityKind::Workload {
                    audience: proxy.audience.clone(),
                },
                _ => IdentityKind::Secret,
            },
        },
    }
}

/// Where a sidecar finds its settings, relative to the sandbox's home.
pub const PROXY_CONFIG_FILE: &str = ".branchyard/credential-proxy.json";

/// The settings file's text.
pub(crate) fn config_text(config: &ProxyConfig) -> Result<String, String> {
    serde_json::to_string_pretty(config).map_err(|e| e.to_string())
}

/// Read a settings file's text.
pub fn parse_config(text: &str) -> Result<ProxyConfig, String> {
    serde_json::from_str(text)
        .map_err(|e| format!("the credential proxy's settings are not understood: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proxy(server: Option<&str>) -> CredentialProxy {
        CredentialProxy {
            server: server.map(str::to_owned),
            audience: "https://by.example".into(),
            ..CredentialProxy::default()
        }
    }

    fn held(site: CredentialSite, server: Option<&str>) -> Holder {
        choose(
            site,
            &proxy(server),
            "https://gw.example/mcp",
            ("blake3:ab", "app/b", 3),
        )
    }

    #[test]
    fn without_a_server_url_every_site_keeps_the_host_proxy() {
        for site in [
            CredentialSite::Host,
            CredentialSite::Guest,
            CredentialSite::Sidecar,
        ] {
            assert_eq!(held(site, None), Holder::Host);
        }
    }

    #[test]
    fn a_host_site_keeps_the_host_proxy_even_with_a_server_url() {
        assert_eq!(
            held(CredentialSite::Host, Some("https://by.example")),
            Holder::Host
        );
    }

    #[test]
    fn a_guest_proves_the_turn_with_a_secret_and_a_sidecar_with_its_workload_identity() {
        let Holder::Sandbox { config, site } =
            held(CredentialSite::Guest, Some("https://by.example"))
        else {
            panic!("a guest runs the proxy");
        };
        assert_eq!(site, CredentialSite::Guest);
        assert_eq!(config.identity, IdentityKind::Secret);
        assert_eq!(config.listen, "127.0.0.1:18900");
        assert_eq!(config.upstream, "https://gw.example/mcp");
        assert_eq!(
            (
                config.manifest.as_str(),
                config.branch.as_str(),
                config.turn
            ),
            ("blake3:ab", "app/b", 3)
        );
        let Holder::Sandbox { config, .. } =
            held(CredentialSite::Sidecar, Some("https://by.example"))
        else {
            panic!("a sidecar runs the proxy");
        };
        assert_eq!(
            config.identity,
            IdentityKind::Workload {
                audience: "https://by.example".into()
            }
        );
        // The settings file holds no secret and reads back.
        let text = config_text(&config).unwrap();
        assert!(!text.to_lowercase().contains("secret"), "{text}");
        assert_eq!(parse_config(&text).unwrap(), config);
        assert!(parse_config("{\"listen\": 1}").is_err());
    }
}
