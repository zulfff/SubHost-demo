# ADR-002: RPC authentication lives in `subhost-rpc`, policy reaches it through `NodeConfig`

- Status: accepted
- Date: 2026-11-18
- Settles: `Q-006`
- Tracks: B (implementation), A (two-line `NodeConfig` seam)

## Context

S5.1 puts a bearer token and a rate limit in front of the node's JSON-RPC server.
Everything else in that task is track B's surface (`subhost-rpc`, `subhost-cli`,
`subhost-faucet`, `explorer`), but a node can only enforce a token if the policy
reaches `RpcServerConfig { listen_addr, max_connections }`, which `Node::run`
builds in `crates/subhost-node/src/lib.rs`. `subhost-cli` builds `NodeConfig`, so
the change crosses the crate boundary in exactly two places.

Two shapes were possible. Track A could own a node-level auth config and pass
individual fields to `subhost-rpc`, or track B could own one struct that the node
forwards untouched. The first duplicates validation in two crates; the second
keeps a single policy type but requires track A to compile against it.

## Decision

Track B owns the policy type and the enforcement; track A forwards it.

```rust
// subhost-rpc::security
#[derive(Clone, Default)]
pub struct RpcSecurityConfig {
    pub auth_token: Option<SecretToken>,
    pub rate_limit_per_sec: Option<u32>,
    pub allow_public: bool,
}

// subhost-node::NodeConfig
pub rpc_security: subhost_rpc::RpcSecurityConfig, // RpcSecurityConfig::default()

// subhost-node::Node::run
security: self.config.rpc_security.clone(),
```

The contract commit is those two lines plus the default in `NodeConfig::new`;
everything else is track B. Track A's six conditions are accepted:

1. `NodeConfig::new` sets `rpc_security: RpcSecurityConfig::default()`, so every
   existing constructor and test keeps compiling and stays default-off.
2. `subhost-rpc::start` — not the caller — refuses a non-loopback bind unless
   `allow_public` **and** a token are both set (`RpcServeError::BindPolicy`).
3. A non-loopback refusal has a test that binds a routable address, so the rule
   cannot regress into a comment.
4. The token is a newtype with a BLAKE3 digest, a constant-time comparison
   (`subtle`), a redacted `Debug`, and `zeroize` on drop. `secrecy` is not used.
5. The rate limit is per connection. jsonrpsee 0.24 does not expose the peer
   address to HTTP middleware, so a per-IP bucket is not implementable on this
   version without a custom transport; the limitation is documented instead of
   implied. Re-verify when jsonrpsee 0.26 lands (dependabot #15).
6. The faucet, the explorer, and `subhost-cli` all send the bearer token and
   explain a 401/429 with the environment variable or flag that fixes it.

## Consequences

- Authentication is opt-in and defaults to today's behaviour: a node with no
  flags still binds loopback and serves unauthenticated callers.
- `--rpc-auth-token-file` is the only secret input; the token is read from a file
  (or `SUBHOST_RPC_AUTH_TOKEN_FILE`) so it never appears in a process list.
- Rejection happens in an HTTP middleware layer before the JSON-RPC handler, and
  answers with a JSON-RPC error body (`-32000` for auth, `-32005` for the limit)
  plus `WWW-Authenticate: Bearer` / `Retry-After`.
- WebSocket authentication is checked at the upgrade handshake only; an
  already-upgraded socket is not re-checked when the token file changes.
- There is no in-process TLS. jsonrpsee 0.24.11 has no server-side TLS feature,
  so the documented deployment is a TLS-terminating reverse proxy in front of a
  loopback bind. `--rpc-allow-public` without a proxy is a deliberate, logged
  choice, not a supported edge deployment.
- A per-connection limit means one client cannot exceed the configured rate, but
  many connections multiply the effective rate. Connection count is still
  bounded by `--max-connections`.

## Alternatives

- **Per-IP rate limiting now.** Rejected: jsonrpsee 0.24.11 does not expose
  `remote_addr` to `set_http_middleware`, so it would need a custom transport or
  an upgrade. Recorded as a follow-up rather than faked with a shared bucket.
- **In-process TLS.** Rejected for this stage: no server-side TLS in the pinned
  jsonrpsee version. A reverse proxy is documented and tested manually instead.
- **Node-owned auth config.** Rejected: it splits validation between
  `subhost-node` and `subhost-rpc` and lets a caller bypass the bind policy.
