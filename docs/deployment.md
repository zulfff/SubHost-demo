# Deployment

## JSON-RPC behind a TLS reverse proxy

`subhost-rpc` does not terminate TLS itself. The supported public deployment keeps
JSON-RPC bound to loopback and terminates TLS at a reverse proxy on the same host
or trusted network. Do not publish the node's plain HTTP port directly to the
internet.

Create a token file with at least 16 bytes of secret material and restrict its
permissions. For example:

```sh
umask 077
openssl rand -hex 32 > ./rpc-token
chmod 600 ./rpc-token
```

Start the node on loopback with token authentication enabled; a rate limit is
optional:

```sh
subhost node --listen 127.0.0.1:8545 --data-dir ./data \
  --rpc-auth-token-file ./rpc-token --rpc-rate-limit 50
```

Configure the reverse proxy to accept HTTPS, forward requests to
`http://127.0.0.1:8545`, and preserve the `Authorization: Bearer <token>` header.
If the proxy authenticates users and injects the node token itself, ensure the
proxy is the only path to the node and that unauthenticated callers cannot reach
the upstream. Keep the token file readable only by the service account.

`--rpc-rate-limit` is per connection, not per IP; multiple connections multiply
the effective rate. WebSocket authentication is checked only during the upgrade.
The metrics endpoint has neither authentication nor TLS, so keep it on loopback
or protect it separately. `--rpc-allow-public` plus a token permits a non-loopback
JSON-RPC bind, but does not provide TLS and is not the recommended edge setup.

See [README security posture](../README.md#security-posture) for the flags and
client behavior, and [manual testing](manual-testing.md) for verification steps.
