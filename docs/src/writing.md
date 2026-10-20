# POST /spa actions

All POST actions below require a bearer token. For example:

```sh
curl http://localhost:2236/spa \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <token>' \
  -d '{"event":"create_collection","id":"Request1","data":{"name":"notes","listed":"true"}}'
```

## create_collection

```json
{"event":"create_collection","id":"Collection1","data":{"name":"notes","listed":"true"}}
```

`name` is required, nonblank, at most 256 UTF-8 bytes, and unique per user. `listed` is
optional and defaults to `"false"`. Success returns HTTP 201 with `data:
{"uuid":"<collection_uuid>"}`.

Listed collections appear in public listings and publish a `create_collection_recv`
event. Unlisted collections can be read/subscribed to by UUID but are hidden from public
listings and collection name lookup.

## create_document

```json
{
  "event":"create_document",
  "id":"Document1",
  "data":{
    "collection":"notes",
    "name":"first",
    "type":"post",
    "content":{"text":"hello","nested":{"key":"value"}}
  }
}
```

| Field | Rules |
| --- | --- |
| `collection` | Your collection's **name**, nonblank and at most 256 UTF-8 bytes |
| `type` | Required, nonblank, at most 64 UTF-8 bytes |
| `content` | Required object containing strings or nested objects |
| `name` | Optional, at most 128 UTF-8 bytes, unique within the collection |

Multiple unnamed documents are allowed. Name limits count the original UTF-8 bytes,
including when JSON escaping is needed for quotes, backslashes, or control characters.

`refs` accepts up to 64 distinct document UUIDs under consecutive numeric keys starting
at `"0"`. Omit it or send `{}` for no references.

Targets must exist in listed collections, but can belong to any account. A missing
target returns HTTP 404. A target in an unlisted collection returns HTTP 400 with
`referenced documents must belong to listed collections`, even for the owner. Invalid UUIDs, duplicate targets, missing numeric keys, or more
than 64 references return HTTP 400. Rejected writes leave no document or index entries
and consume no sequence number.

```json
"refs": {
  "0": "<first_document_uuid>",
  "1": "<second_document_uuid>"
}
```

Documents in unlisted collections can reference listed documents. Their outgoing
references are stored and returned by reads, but are excluded from reverse lookups.
Documents in unlisted collections cannot be reference targets.

This restriction applies to new document creations. Existing references are unchanged.

Success returns HTTP 201 with `data: {"uuid":"<document_uuid>","seq":"123"}`. Sequences
increase globally across all document creations, including unlisted ones. Collection
creations have no sequence number.

### Safe retries

To retry a document creation safely, send an optional `Idempotency-Key` HTTP header:

```sh
curl http://localhost:2236/spa \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <token>' \
  -H 'Idempotency-Key: <unique-key-for-this-write>' \
  -d '{"event":"create_document","id":"Attempt1","data":{"collection":"notes","type":"post","content":{"text":"hello"}}}'
```

Generate a new random key for each logical creation and save it with the pending
request. Reuse that key and document data when retrying a timeout, disconnect, or
ambiguous server failure. Keys must contain 1-128 printable ASCII characters without
spaces and must be supplied in exactly one header field.

Keys belong to an account, so you can retry with a different valid token for that
account. For 24 hours after creation, the same key and data return HTTP 201 with the
original UUID and sequence. No second document or socket event is created. The reply
echoes the current attempt's `id`.

Object property order does not matter, and omitted `refs` matches `{}`. Reference order
and all other validated fields must match. Different data with a live key returns HTTP
409.

Failed requests do not reserve keys. Successful retry records are saved with the
document and survive restarts. They expire 24 hours after creation; retries do not
extend that window. After expiry, reusing the key can create a new document. Cleanup
runs about once a minute, but expiry takes effect immediately.

`Idempotency-Key` applies only to `create_document`. Without it, repeating a request can
create another document. The event `id` only correlates replies. If a crash interrupts
socket publication after a write is saved, recover listed documents through catch-up.

## send

```json
{"event":"send","id":"Message1","data":{"to":"<recipient_account_id>","content":{"text":"hello"}}}
```

`to` is the recipient's account ID. `content` is required and accepts a string or an
object containing strings and nested objects.

The server publishes `send_recv` to the recipient's connected sockets. HTTP 200 with
empty data means at least one socket queued the message; it does not guarantee delivery.
Messages are not stored. A missing recipient returns HTTP 404; an offline recipient
returns HTTP 409.

## revoke_token

```json
{"event":"revoke_token","id":"Logout1","data":{}}
```

Revoke the token in the Authorization header. Success returns HTTP 200 with empty data
and signals sockets using that token to close. Other tokens for the same user remain
valid. Repeating logout for an absent or expired well-formed token succeeds. Subsequent
authenticated requests and new sockets using the revoked token fail.
