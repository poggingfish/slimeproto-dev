# Server configuration

## Startup settings

The server takes these environment variables:

| Variable | Default | Purpose |
| --- | --- | --- |
| `SLIMEPROTO_BIND_ADDRESS` | `0.0.0.0:3000` | Address and port to listen on |
| `SLIMEPROTO_DATA_PATH` | `data/slimeproto-identities-v1` | LMDB database directory |
| `SLIMEPROTO_CONFIG` | `slimeproto.json` | Path to the JSON configuration file |

Relative paths use the process's working directory. For example:

```sh
SLIMEPROTO_BIND_ADDRESS=127.0.0.1:3000 \
SLIMEPROTO_DATA_PATH=/srv/slimeproto/data \
SLIMEPROTO_CONFIG=/etc/slimeproto/server.json \
./slimeproto
```

Create the config file and give the server process write access to the data directory
before using these paths. The server creates a missing data directory when its parent
permissions allow it.

The binary serves HTTP. For HTTPS, put it behind a TLS reverse proxy that also forwards
WebSocket upgrades for `/ws`. If you want rate limits to use the original client IP, add
the proxy's exact address to `trusted_proxy_ips` and configure the proxy to append or
overwrite `X-Forwarded-For` correctly.

## JSON configuration

Create `slimeproto.json` in the working directory, or point `SLIMEPROTO_CONFIG` to
another JSON file. Restart after changing settings.

Without `slimeproto.json`, the server uses these defaults. If you explicitly set
`SLIMEPROTO_CONFIG`, that file must exist. Invalid types and unknown fields fail
startup.

```json
{
  "database_map_size_bytes": 1073741824,
  "registration_keys": null,
  "max_storage_per_user_bytes": 104857600,
  "max_requests_per_minute_per_ip": 600,
  "max_requests_per_minute_per_user": 300,
  "trusted_proxy_ips": []
}
```

| Setting | Meaning |
| --- | --- |
| `database_map_size_bytes` | Total LMDB map capacity in bytes, default 1 GiB. Must be a positive multiple of 1 MiB (1048576 bytes). Increase this value and restart to expand an existing database. |
| `registration_keys` | `null` allows open registration; `[]` disables account creation; an array of strings requires one matching invite key. Applies to registration and identity import. |
| `max_storage_per_user_bytes` | Maximum combined serialized collection/document bytes per account. Default: 100 MiB. `null` disables the cap; `0` prevents collection/document creation. |
| `max_requests_per_minute_per_ip` | Requests from each client IP in a rolling 60-second window. `null` disables this limit; `0` rejects all application requests. |
| `max_requests_per_minute_per_user` | Authenticated requests from each account across all tokens, connections, and IPs in a rolling 60-second window. `null` disables this limit; `0` rejects all authenticated requests and successful logins/imports. |
| `trusted_proxy_ips` | Exact proxy IP addresses permitted to supply `X-Forwarded-For`. Empty by default; forwarded headers from other peers are ignored. |

## Database capacity

`database_map_size_bytes` sets capacity for the whole database, including all accounts,
indexes, sessions, and LMDB overhead. It reserves virtual address space without
preallocating that much disk.

When the map fills, writes return HTTP 507:

```text
database capacity exceeded; increase database_map_size_bytes and restart
```

Increase the setting and restart with the same data path. Records and live sessions are
retained, and reads continue to work while the map is full. An upgrade may also need
more capacity to finish its startup migrations.

If the disk itself fills, writes return HTTP 507 with `database storage is full; free
disk space and retry`. Free disk space before retrying; increasing map capacity will not
help.

## Invite keys

Invite keys apply to both registration and identity import. Set `registration_keys` to
`null` for open registration, `[]` to disable it, or an array of reusable invite
strings.

Keys match exactly, including whitespace and Unicode representation. There is no length
limit; an empty string is valid if explicitly configured. Keep the active config
private.

## Rate limits

The IP and account limits use rolling 60-second windows. They count HTTP requests,
WebSocket handshakes, and incoming text or binary messages, including malformed
commands. Ping/pong, outgoing publications, and CORS preflight requests do not count.

The IP limit runs first. A request that passes it consumes an IP slot even if the
account limit later rejects it. Successful credential checks on login or identity import
consume an account slot; invalid credentials consume only an IP slot. Public requests
without valid credentials use only IP slots, even when reading another account's data.

HTTP rejection returns status 429 with:

```json
{"fakywaky":"request rate limit exceeded"}
```

The response includes `Retry-After` in seconds and `Cache-Control: no-store`. It has no
`/spa` action envelope. Browsers can read `Retry-After`.

A socket exceeding either limit closes with code 1008 and a retry delay in its close
reason. Wait for that delay before reconnecting.

Counters reset on restart. Separate server processes keep separate counters.

## Trusted proxies and client IPs

By default, IP limits use the TCP peer address. IPv4-mapped IPv6 addresses are
normalized to IPv4.

Only peers in `trusted_proxy_ips` can supply `X-Forwarded-For`. For a trusted peer, the
server walks the header from right to left through trusted proxies and uses the first
untrusted address. Missing or malformed headers fall back to the peer address.

List exact IP addresses, not CIDR ranges. Configure each proxy to append or overwrite
forwarding headers correctly.

## Per-account storage quotas

`max_storage_per_user_bytes` counts the stored JSON bytes of collections and documents:
names, metadata, references, content, and signatures. Accounts, sessions, retry records,
indexes, and LMDB page overhead are excluded. This quota measures account data, not disk
usage.

Usage counters persist across restarts and update with each successful write. Existing
records are counted once at startup. Failed writes and idempotent retries add nothing.
Reads, identity exports, and transient messages use no storage quota.

Exceeding the quota returns HTTP 507 in the action error envelope. Lowering the cap
preserves existing data but blocks creation until the cap allows it.

To check your usage, send an authenticated request:

```text
GET /spa?event=get_storage_usage&id=Usage1&user=<account_id>
```

The response data contains string byte counts:

```json
{"used_bytes":"1234","max_bytes":"104857600"}
```

`max_bytes` is `""` when unlimited. The token must belong to the requested account:
another account gets 403, and missing or invalid authentication gets
401. Responses use `Cache-Control: no-store`.

## Authentication format

Authenticated HTTP actions use:

```http
Authorization: Bearer <token>
```

UUIDs and tokens use the 37-character lowercase format
`01a0f9927621:73b1:881c:2429:6332:2eb6`.
