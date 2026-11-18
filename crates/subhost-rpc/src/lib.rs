//! JSON-RPC server and the single-node block producer behind it.
//!
//! Scope: an honest, deliberately small Ethereum-compatible subset for a single
//! local node. A submission is admitted to the pool and the next block is sealed
//! with every pending transaction that is currently executable (per-sender
//! nonce-contiguous, bounded by the block gas limit), so `eth_sendTransaction`
//! either returns a hash for a committed block or an error — there is no pending
//! limbo.
//!
//! Security notes:
//! - `eth_sendTransaction` requires an ed25519 public key and signature; the
//!   signature is verified over the unsigned encoding and the public key must
//!   hash to the `from` address, so a caller cannot spend another account.
//! - Blocks arriving from a peer go through [`RpcState::accept_block`], which
//!   re-verifies parent, height, every transaction signature, both roots, and the
//!   resulting state root before any state changes. With a validator registry it
//!   also requires exactly one signature from a registered producer over the block
//!   signing payload. [`RpcState::with_validators`] enables that check without a
//!   signing key, so a follower verifies peer blocks.
//! - The whole submit path holds one commit lock, and state is only swapped in
//!   after the ledger write succeeds, so a failed persist cannot leave the
//!   in-memory ledger ahead of disk.
//! - The loopback bind is the default and a non-loopback bind is refused unless
//!   `--rpc-allow-public` and a bearer token are both configured. With a token,
//!   every HTTP request must carry `Authorization: Bearer <token>` (constant-time
//!   check); a configured rate limit is a token bucket per connection. TLS is not
//!   served in process - see `docs/deployment.md` for the reverse-proxy mode.

mod block;
mod methods;
mod observability;
mod security;
mod state;

#[cfg(test)]
mod test_support;

pub use block::BlockAuthority;
pub use methods::{RpcServeError, RpcServer, RpcServerConfig};
pub use security::{
    BindPolicyError, RpcSecurityConfig, SecretToken, SecurityError, TokenError, MIN_TOKEN_BYTES,
};
pub use state::{BlockEvent, RpcError, RpcState};
