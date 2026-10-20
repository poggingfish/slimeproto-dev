# HTTP endpoints

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/` | Returns the text `Hello, World!` |
| POST | `/register` | Create an account |
| POST | `/token` | Log in using an account ID and password |
| POST | `/identity/export` | Export a password-encrypted identity backup |
| POST | `/identity/import` | Restore an identity on another server |
| POST | `/spa` | Create collections/documents, send messages, or revoke a token |
| GET | `/spa` | Read data using query parameters |
| GET / WebSocket | `/ws?token=<token>` | Open an authenticated socket |

Application error messages, including validation errors, use lowercase text.

JSON POST requests use `Content-Type: application/json`. The maximum request body is
normally 16 KiB. Registration/import limits grow automatically when longer invite keys
are configured.

# Requests and responses

Every `/spa` request requires `event` and `id`. IDs are client-provided correlation IDs
containing 1-64 ASCII letters or digits. Responses echo the ID.

POST requests send JSON:

```json
{"event":"create_collection","id":"Request1","data":{"name":"notes","listed":"true"}}
```

GET requests put fields directly in the query string, without a JSON body:

```text
/spa?event=get_identity&id=Request2&user=<account_id>
```

Success events are `<action>_ok`; errors are `<action>_err`:

```json
{"event":"create_collection_ok","id":"Request1","data":{"uuid":"<collection_uuid>"}}
```

```json
{"event":"create_collection_err","id":"Request1","data":{"fakywaky":"collection name already exists"}}
```

Invalid IDs or unknown/missing events produce `generic_err`. Invalid IDs return an empty
response ID. GET also rejects duplicate/unknown query parameters and malformed query
encoding.

Action `data` supports strings and nested objects. Use `"true"`/`"false"` and string
sequence numbers. Arrays, JSON booleans, numbers, and null are not supported inside
action data.

## Common HTTP status codes

| Status | Meaning |
| --- | --- |
| 200 | Successful read, message send, or token revocation |
| 201 | Account, collection, or document created |
| 400 | Invalid fields, IDs, limits, or cursors |
| 401 | Missing, invalid, expired, or revoked required token |
| 403 | Another user's private collection listing requested |
| 404 | User, collection, or document not found; also hidden collection name lookup |
| 409 | Duplicate collection/document name, reused idempotency key with different data, existing identity import, or recipient offline |
| 413 | Request body too large |
| 429 | IP or account request limit exceeded; respect `Retry-After` |
| 500 | Internal server error |
| 503 | Password/backup workers busy; respect `Retry-After` |
| 507 | Account quota, shared database capacity, or disk space exhausted |
