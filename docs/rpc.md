# JSON-RPC methods and compatibility

The node serves JSON-RPC 2.0 over HTTP and WebSocket. See
[deployment.md](deployment.md) for bearer authentication, rate limits and TLS
through a reverse proxy. Requests and responses retain their JSON-RPC IDs; batch
notifications produce no response. The maximum request is 256 KiB, the maximum
response is 1 MiB, and a batch contains at most ten requests.

## Block queries

`eth_getBlockByHash` accepts `["0x<64 hex digits>", false]`. Set the second
argument to `true` for transaction objects; omit it for transaction hashes.
`eth_getBlockByNumber` uses the same second argument, with a block tag or canonical
hex quantity first. An incorrectly typed flag or extra argument returns `-32602`.

Both methods return the same block representation. Unknown and pruned blocks
return `null`. Hash lookup scans retained blocks; there is no persistent hash
index yet. The initial account allocation has no synthetic height-zero block.
`earliest` means the oldest retained block. `latest` and `pending` refer to the
committed tip; existing `safe` and `finalized` aliases also resolve to that tip
and do not provide a separate finality guarantee.

## Fee queries

`eth_gasPrice` and `eth_maxPriorityFeePerGas` return the mempool's minimum accepted
gas price as a hex quantity (currently `0x1`). There is no EIP-1559 base-fee market
yet. These values are admission recommendations, not predictions of congestion.

`eth_feeHistory` accepts `[blockCount, newestBlock, rewardPercentiles]`:

- `blockCount` is a canonical hex quantity from `0x1` to `0x400` (1024).
- `newestBlock` accepts the same tags/heights as block queries. A requested range
  clips at the oldest retained block. A newest height outside retained history
  is an invalid parameter. On a fresh ledger, tags and `0x0` return empty history.
- `rewardPercentiles` is optional. When supplied, it is an array of at most 100
  strictly increasing numeric values between 0 and 100, inclusive.

The response includes `oldestBlock`, `baseFeePerGas` and `gasUsedRatio`. Base fees
are zero for this pre-EIP-1559 model; a nonempty range includes one additional
base-fee entry for the block after the newest returned block. Gas ratios come
from each retained block's recorded gas usage and limit. When percentiles are
nonempty, `reward` contains gas-weighted transaction gas prices in ascending
percentile order. Recorded gas consumption currently equals each transaction's
gas limit. These effective-tip samples do not assert that fees are paid to a
producer. An empty percentile list omits `reward`; empty history returns empty
arrays and `oldestBlock: "0x0"`.

```json
{"jsonrpc":"2.0","id":1,"method":"eth_feeHistory","params":["0x2","latest",[0,50,100]]}
```

The fee arrays describe a consistent snapshot of the retained block index, even
while another thread commits a new block. Historical base fees will need the
ledger and fee-market contract planned in S6.

## Sync and unsupported methods

`eth_syncing` returns `false`: no block-sync worker is implemented yet. This
reports the absence of an active sync operation; it does not certify that the
node agrees with its peers, is ready, or knows the network's newest height. The
method and `eth_maxPriorityFeePerGas` accept no arguments.

`eth_getLogs` and `eth_subscribe` return error `-32004` with a documented
unsupported-method message. Contract execution, log filtering and subscriptions
are deferred to S10. Other unregistered methods return `-32601`.

## Error codes

| Code | Meaning |
| --- | --- |
| `-32700` | Invalid JSON. |
| `-32600` | Invalid JSON-RPC request. |
| `-32601` | Unknown method. |
| `-32602` | Invalid parameters, address, hash, chain ID or transaction signature. |
| `-32603` | Internal or persistence failure; local paths and details are suppressed. |
| `-32000` | Rejected transaction/block, unknown account proof, or authentication failure. |
| `-32004` | Explicitly unsupported logs or subscription method. |
| `-32005` | Configured connection rate limit exceeded (HTTP 429). |

Authentication failure uses HTTP 401. Other method errors use JSON-RPC response
objects with an `error` member and no `result`. Unknown receipt/transaction/block
lookups use a successful `null` result instead of an error.

## Sources

Verified 2026-11-18:

- [Ethereum execution APIs: fee market](https://github.com/ethereum/execution-apis/blob/main/src/eth/fee_market.yaml)
  specifies zero pre-EIP-1559 base fees, a next-block fee entry and gas-weighted rewards.
- [Ethereum execution APIs: client](https://github.com/ethereum/execution-apis/blob/main/src/eth/client.yaml)
  specifies a sync object or `false`.
- [EIP-1474](https://eips.ethereum.org/EIPS/eip-1474) specifies hex quantities and
  Ethereum RPC error codes, including `-32004`.
- [JSON-RPC 2.0](https://www.jsonrpc.org/specification) specifies request IDs,
  errors, batches and notifications.
