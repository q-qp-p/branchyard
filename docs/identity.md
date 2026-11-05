# Native identity and connector accounts

Branchyard separates the server caller, the process's cloud identity, and the account used by an upstream connector. Granting one does not grant the others. A Google Storage IAM permission does not become a Branchyard server scope. A branch's connector grant does not grant access to another person's Google document.

## Native Google access

Sync obtains Google API credentials through the official `google-cloud-auth` provider. Set `GOOGLE_APPLICATION_CREDENTIALS` to an ADC configuration when needed, or run with an attached Google Cloud identity. Official ADC discovery supports external-account federation and local user or impersonation configurations instead of assuming every file contains a private key. No `gcloud` subprocess or custom STS exchange is used. The existing `GOOGLE_OAUTH_ACCESS_TOKEN` override remains a fixed, externally managed token; Branchyard cannot refresh it.

GCS and Cloud KMS use the provider's access token with cloud-platform scope. Native IAM still decides whether each API request is permitted. The blocking storage interface owns a separate runtime thread with a bounded request queue and request deadlines, so calling it from a server Tokio runtime does not nest runtimes. The provider owns cache expiry and background refresh. Error messages identify the credential boundary without printing the provider's secret-bearing error response.

Explicit emulator access remains anonymous. Credential discovery is lazy: opening a store does not read an ADC file until an authenticated request needs a token. Tests construct fake credential sources and loopback endpoints.

## Google ID-token admission

A server can accept Google-signed ID tokens alongside its existing configured credentials. Configure an exact audience and exact issuer/subject bindings:

```json
{
  "google_identity": {
    "audience": "https://branchyard.example",
    "bindings": [
      {
        "issuer": "https://accounts.google.com",
        "subject": "123456789012345678901",
        "principal": {
          "name": "build-worker",
          "tenant": "engineering",
          "scopes": ["read", "run"],
          "repos": ["application"]
        }
      }
    ]
  }
}
```

The official verifier checks the Google signature, trusted Google certificates and configured audience. Branchyard also requires issue/expiry timestamps and enforces not-before when present. A presented token is bounded in size and verification has a deadline. Unknown subjects, alternate issuers and unbound audiences are rejected. Bindings do not match email addresses, token scopes, wildcard subjects or request-supplied tenants. A Google API access token is not a server ID token.

Enabling Google admission initializes the ring TLS provider when the process has no default provider, including when the verifier is used before server startup. An already installed TLS provider is preserved.

The resulting principal uses the configured scopes and repository policy. The API and main-listener `/metrics` apply the same Google admission, with `admin` still required for metrics. Google caller credentials do not authorize the companion's pairing or push routes; public pairing-code redemption remains a separate flow. A Google-only configuration does not create a fallback static token file. Cloud IAM role synchronization is outside this implementation. Enterprise OIDC is a separate path: see [Federated identity](#federated-identity-workforceworkload-identity-federation-oidc).

The principal's durable `origin` records the verified issuer, subject and server audience without the bearer. Authenticated device pairing records `origin.kind = "paired"`, preserving the original subject and issuer through further pairing. Each device keeps its own name and narrowed scopes and repository allowlist. A request, push delivery or queued operation rechecks those grants against the original issuer's current policy. Removing that configured issuer or Google binding, changing its tenant or audience, or narrowing it below the device's grants refuses the device. A queued operation reports `identity_binding_revoked`. Renaming a Google binding's display name does not change a paired device's subject. Nested pairing checks the original issuer and the leaf token. It does not retain intermediate token identifiers or inherit their later revocation.

Captured service callbacks also recheck the exact credential that authenticated their caller. Revoking that paired token remains effective even when another live token has the same principal. A trusted internal caller with no captured credential receives a current-policy check, without proof of a specific paired token. Normal paired requests and callbacks enforce token expiry. Existing durable operation admission retains its identity after token expiry and still checks issuer policy. Locally created operator links with no recorded issuing origin keep their existing expiry and revocation behavior. Such a link can issue a descendant only if its subject has a current configured issuer. Link creation rejects an unbound issuer before saving a code and points to `by token new --link` to create the target device link directly. Older records default to `origin.kind = "configured"`. This provenance check does not enable Google machine credentials to issue browser pairing links.

## Behind IAP

Identity-Aware Proxy can sit in front of the console and the API, so people sign in with their Google Workspace or Cloud Identity account and no bearer token reaches a browser. IAP signs an assertion for every request it forwards, in the `x-goog-iap-jwt-assertion` header. Configure `iap` to have the server verify it:

```json
{
  "iap": {
    "audience": "/projects/123456789/global/backendServices/987654321",
    "tenant": "engineering",
    "scopes": ["read"],
    "groups_from": "cloud_identity",
    "bindings": [
      {"match": "oncall-lead@example.com", "name": "oncall-lead", "tenant": "ops", "scopes": ["read", "run", "admin"]},
      {"match": "group:eng@example.com", "name": "engineer", "scopes": ["read", "run"]},
      {"match": "domain:example.com", "name": "staff"}
    ]
  }
}
```

The server checks the ES256 signature against Google's published IAP keys (`https://www.gstatic.com/iap/verify/public_key-jwk`, cached for an hour and refetched, at most once a minute, when a key id is unknown), the issuer `https://cloud.google.com/iap`, the audience exactly, and `exp` and `iat` with ten seconds of skew. The audience is `/projects/NUMBER/global/backendServices/ID` for a load balancer backend, or `/projects/NUMBER/apps/PROJECT` for App Engine and Cloud Run. `keys_url` replaces the key location; it exists for tests and mirrors.

A request that carries the header is authenticated by it when `iap` is configured, and by nothing else. If the assertion fails, the request gets `401` even when it also carries a valid bearer token. Without `iap` configured the header is ignored and the request is judged by its other credentials. Scripts and the CLI that do not pass through IAP keep their bearer tokens.

A verified assertion is mapped to a principal by the first binding that matches, in order:

- an exact email address (case-insensitive);
- `domain:example.com`, the domain of the email;
- `group:eng@example.com`, a group the person belongs to.

A binding gives the principal's `name`, `tenant`, `scopes` and `repos`. What it omits comes from the provider (`tenant`, `scopes`), then from the defaults (the default tenant, `read`). A verified person with no matching binding gets `403 identity_not_bound`; there is no implicit access.

IAP assertions carry no groups. With `groups_from: "cloud_identity"` the server asks the Cloud Identity Groups API (`memberships:searchTransitiveGroups`, so nested groups count) for the person's groups, using the access token of the server's own service account from the metadata server. Grant that account the Groups Reader role in Cloud Identity. The answer is cached for five minutes per email. If the lookup fails, the request gets `503 identity_unavailable`; the server does not guess a smaller set of groups. The groups land in the principal's `groups`.

The principal's `origin` is `iap` with the verified email and IAP subject. Its connector subject is `iap:` followed by SHA-256 of the issuer and subject, so a person keeps their connections when their email changes, and a person never shares a subject with an OIDC identity. The `iap:` prefix is reserved like `google:`. Federated callers cannot use companion pairing or push subscriptions.

### GKE Gateway with IAP

Enable IAP on the backend with a `GCPBackendPolicy`, and give the policy's OAuth client secret to the cluster:

```yaml
apiVersion: networking.gke.io/v1
kind: GCPBackendPolicy
metadata:
  name: branchyard
spec:
  default:
    iap:
      enabled: true
      oauth2ClientSecret:
        name: iap-oauth-client
      clientID: 123456-abc.apps.googleusercontent.com
  targetRef:
    group: ""
    kind: Service
    name: branchyard
```

Route the Gateway to the `branchyard` Service as usual. The audience is the backend service's resource path. Read it with `gcloud compute backend-services describe BACKEND --global --format='value(id)'` and put the project number in front: `/projects/PROJECT_NUMBER/global/backendServices/ID`. For Cloud Run behind IAP use `/projects/PROJECT_NUMBER/apps/PROJECT_ID`. Restrict the pod's ingress to the load balancer, so no one can reach the port with a forged header. The server trusts the signature, not the network, but a direct caller would simply fail verification.

## Federated identity (Workforce/Workload Identity Federation, OIDC)

A server also accepts bearer ID tokens from standard OIDC issuers. That covers the tokens a Workforce Identity Federation pool issues to people (through the pool's OIDC or SAML provider), the ID tokens a workload gets under Workload Identity Federation, and any other OIDC identity provider, such as Okta, Entra ID or Keycloak. Configure each issuer in `oidc_issuers`:

```json
{
  "oidc_issuers": [
    {
      "issuer": "https://login.example-idp.test",
      "audience": "//iam.googleapis.com/locations/global/workforcePools/eng-pool/providers/okta",
      "jwks_url": "https://login.example-idp.test/.well-known/jwks.json",
      "subject_claim": "sub",
      "email_claim": "email",
      "groups_claim": "groups",
      "tenant": "engineering",
      "scopes": ["read"],
      "bindings": [
        {"match": "group:platform@example.com", "name": "platform", "scopes": ["read", "run"]},
        {"match": "domain:example.com", "name": "staff"}
      ]
    }
  ]
}
```

The server selects the issuer by the token's `iss` claim, then verifies the signature (RS256 or ES256) against that issuer's keys, the exact issuer and audience, and `exp`, `iat` and `nbf` with ten seconds of skew. Keys are cached as for IAP. A token whose `iss` names no configured issuer is not an OIDC token to this server, and the request goes on to the other credential kinds. A token that names a configured issuer and fails verification gets `401`; one that verifies but matches no binding gets `403 identity_not_bound`. An email counts only when the token's `email_verified` is not `false`.

Claims are read by the names configured: `subject_claim` (default `sub`), `email_claim` (default `email`) and `groups_claim` (default `groups`, an array of strings or one string). Bindings, defaults, groups lookup and errors work as under [Behind IAP](#behind-iap); `groups_from: "cloud_identity"` is the fallback when the token carries no groups claim. The origin is `oidc` with the verified issuer and subject, and the verified email when the token carries one; the connector subject is `oidc:` followed by SHA-256 of the issuer and subject.

A queued operation of a federated caller is rechecked against the binding sign-in would choose now: the first whose `match` covers the email and groups captured at sign-in. Only that binding's tenant, scopes and repositories count. Another binding with the same permissions does not keep an identity whose own binding was removed: the identity falls to the next binding that matches it, or to none, and the operation is refused `identity_binding_revoked` if that allows less. Groups read from Cloud Identity at sign-in are read again when a worker claims the operation, once they are older than the lookup's five-minute cache: see [fresh groups](#fresh-groups-on-a-recheck). Groups a token's claim carried are the ones captured at sign-in; removing a person from one takes effect at their next sign-in. An OIDC origin recorded before the email was kept matches only `group:` bindings.

#### Fresh groups on a recheck

An IAP or federated principal whose groups came from Cloud Identity records when they were read (`groups_at_ms` in its `origin`; absent when a token's claim gave them, or the provider has no lookup). A worker that claims a queued operation of such a person checks that time. Groups younger than the lookup's five-minute cache are used as captured. Older ones are looked up again, through the same cache, with the worker's own service account, and the binding is chosen with the fresh groups, as sign-in would choose it. If that binding allows less than the person was admitted with (they left the group that bound them to `run`, or to any binding at all), the operation is refused `identity_binding_revoked`; a person removed from a group loses its access without signing in again. If the lookup fails the operation is refused `503 identity_unavailable`; the worker never guesses a smaller or larger set. A principal admitted by a configured token, by Google ID-token bindings or by a claim carries no lookup time and is checked as before. The blocking lookup runs on the worker's executor thread, so the recheck stays synchronous for non-federated callers.

For a Workforce Identity Federation pool, set `audience` to the pool provider's resource name or to the client ID your IdP issues tokens for, whichever your tokens carry in `aud`, and `jwks_url` to the IdP's key set. Map the IdP's group attribute into a claim the token carries, or leave `groups_claim` unset and use `groups_from`, which resolves the person's email in Cloud Identity. For Workload Identity Federation, bind the workload's subject by putting its email or a group in a binding; a workload with no email needs a `groups_claim` that names its role.

A server with only `iap` or `oidc_issuers` and no tokens does not create a fallback token file.

### Evidence

Hermetic evidence, 5 November 2026 (fresh groups): `cargo test -p branchyard-server --lib -- federated work::` passed. The tests record the lookup time at sign-in (and none for a claim's groups), then recheck a principal whose groups are ten minutes old against a Cloud Identity mock that still lists the group (one lookup, then the cache), no longer lists it (`identity_binding_revoked`), answers 503 (`identity_unavailable`), and against groups younger than the cache (no lookup); a configured token is left to the ordinary recheck. Not done: a real Cloud Identity, a clock past the cache (the tests age the recorded time instead of waiting), and a worker on another host.

Hermetic evidence, 4 November 2026: `cargo test -p branchyard-server --lib federated` passed. The tests sign IAP assertions (ES256) with a key generated in the test and OIDC tokens (RS256 from the repository's fixed test key, and ES256) and serve the keys and Cloud Identity answers from a local mock HTTP server. They cover valid tokens, wrong audience, expiry, wrong issuer, unknown key id, a missing binding, groups from a claim and from a mocked Cloud Identity (with paging and the five-minute cache), an unreachable key set, and an IAP header with `iap` unset, through the unit API and through the HTTP server. Not done: a real IAP-protected backend, a real Workforce pool, the live Cloud Identity Groups API and the metadata server's token route, and any model call.

## Subjects at the connector boundary

A static caller keeps its existing connector subject. The `google:` prefix is reserved; configured or paired static principals using it must be renamed, and queued work with that old ambiguous static name is refused. A Google-admitted caller uses `google:` followed by SHA-256 of the compact JSON array containing its verified external issuer and subject. This bounded subject distinguishes external callers even when configured display names coincide. Idempotency uses the derived subject too. Existing name-based connector/model/network ceilings and approval policies are applied to the corresponding derived subjects, so changing the identity representation cannot bypass them.

Branchyard signs this subject with its own issuer and `by_tenant` in turn and connect tokens. Anvil's account owner is the authenticated Branchyard issuer, tenant and signed subject. Connect-purpose and turn-purpose operations must resolve the same namespace. Ambiguous old subject-only vault records require an explicit migration; they are not automatically adopted by a newly identified caller.

The branch token remains a broker credential and is never an upstream API or MCP credential. Anvil owns human OAuth consent and refresh, workload credential selection, and protected upstream MCP execution. Missing human consent must not fall back to gateway workload authority. Native IAM, OAuth scopes and resource ACLs remain authoritative on the actual upstream call. Child connector grants can only narrow their parent's authority.

## Evidence

Hermetic evidence, 2026-10-29: `cargo test -p branchyard-server --lib --locked --offline auth::policy::tests` and `cargo test -p branchyard-server --lib --locked --offline companion::link::tests` passed. The tests use local SQLite and constructed provenance to check issuer removal and narrowing, nested pairing, stable subjects, exact captured-token revocation, legacy operator links, paired request verification and push ownership. The queued paired-Google regression failed before the fix because removal of its binding still allowed execution. The unbound-link regression also failed before the fix, then passed after link creation rejected the issuer before saving a code. Not done in these regressions: real Google admission, Google browser pairing, cloud services or model calls.

Hermetic evidence, 2026-10-29: the server and sync library suites, server connector and tenant suites, sync conformance suite, and official-ADC integration test passed with `--locked --offline`. The regression paths use fake ADC, metadata, STS and Google certificate endpoints plus real official provider/verifier code. They exercise the blocking adapter inside a running Tokio runtime, exact identity matching, policy-controlled scopes, durable origin, invalid tokens and the HTTP API/metrics boundary. Not done in those suites: real cloud services or model calls.

Live evidence, 2026-10-29: dedicated GKE workloads exercised official ADC and native Storage IAM allow/deny through the compiled Anvil gateway and a TLS MCP qualification fixture. The complete chain also verified real Google admission, the actual engine-issued connector token, generated SDK, effect proxy, supervised Anvil and the real Storage API. Issuer, tenant and derived subject matched the gateway owner namespace; grant denial made no upstream requests. Wrong audiences and unmapped subjects were rejected. Temporary resources and private validation files were removed. See the [dated record](atomic-knowledge-validation-2026-10-29.md). Not done: live human OAuth consent for Google Workspace or a model-driven turn in that identity qualification.
