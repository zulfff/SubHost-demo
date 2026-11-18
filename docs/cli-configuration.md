# CLI and node configuration

The CLI constructs node settings through `NodeConfig::new(data_dir)`, then
applies the RPC address, connection limit, authentication/rate policy, metrics
address, validator/key settings, fallback chain ID and P2P options exposed by
the current command. Address and RPC security validation occurs before bootstrap.

This keeps the CLI compatible when the node adds configuration fields: fields
that have no CLI consumer yet retain node-owned defaults. A configuration change
that needs new CLI behaviour still requires its shared contract and working flag
consumer; compatibility does not mean the new setting is exposed automatically.

The explicit unsigned-demo `--single-writer` policy and configurable consensus
timeout remain pending the node policy contract. Their CLI and script migration
will follow those landed types. This construction change preserves all current
flag values and does not alter consensus, producer keys or ledger handling.
