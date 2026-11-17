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
//!   resulting state root before any state changes. Block signatures are not
//!   checked yet; S1.3 adds that.
//! - The whole submit path holds one commit lock, and state is only swapped in
//!   after the ledger write succeeds, so a failed persist cannot leave the
//!   in-memory ledger ahead of disk.
//! - The server binds without authentication; it is intended for a loopback or
//!   otherwise access-controlled interface, never a public one.

mod block;
mod methods;
mod state;

#[cfg(test)]
mod test_support;

pub use block::BlockAuthority;
pub use methods::{RpcServeError, RpcServer, RpcServerConfig};
pub use state::{BlockEvent, RpcError, RpcState};
