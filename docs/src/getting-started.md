# Getting started

The examples below use a POSIX shell, `curl`, and `jq`. You need access to a running
Slimeproto server. If you already have a server URL, skip the local startup step and use
that URL for `base` in the examples.

## Start the server

With the server executable in your working directory, run:

```sh
SLIMEPROTO_BIND_ADDRESS=127.0.0.1:3000 ./slimeproto
```

This binds to localhost for the walkthrough. Without the environment variable, the
server listens on `0.0.0.0:3000`. The default configuration allows registration and sets
storage and request limits.

The server creates `data/slimeproto-identities-v1` on first startup. Paths are relative
to the working directory. In another terminal:

```sh
curl http://localhost:3000/
```

You should get `Hello, World!`.

## Create an account and log in

These are local example credentials. Choose your own password for an account you plan to
keep. Registration returns the account ID but does not log you in.

```sh
base=http://localhost:3000

account_id=$(curl --fail-with-body -sS "$base/register" \
  -H 'Content-Type: application/json' \
  -d '{"password":"example-password"}' | jq -er '.account_id')

printf '%s\n' "$account_id"

token=$(curl --fail-with-body -sS "$base/token" \
  -H 'Content-Type: application/json' \
  -d "{\"account_id\":\"$account_id\",\"password\":\"example-password\"}" \
  | jq -er '.token')
```

Save your account ID. You need it with your password to log in again. A token lasts 24
hours. To register on an invite-only server, add `registration_key` to the registration
JSON.

## Create a collection

```sh
collection_id=$(curl --fail-with-body -sS "$base/spa" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $token" \
  -d '{"event":"create_collection","id":"Collection1","data":{"name":"notes","listed":"true"}}' \
  | jq -er '.data.uuid')
```

The collection is listed, so other clients can discover it. Collection names are unique
within your account.

## Write a document

Creation uses the collection's name. Most reads use its UUID.

```sh
document_id=$(curl --fail-with-body -sS "$base/spa" \
  -H 'Content-Type: application/json' \
  -H "Authorization: Bearer $token" \
  -H 'Idempotency-Key: tutorial-first-note' \
  -d '{"event":"create_document","id":"Document1","data":{"collection":"notes","type":"note","content":{"text":"Remember the coffee"}}}' \
  | jq -er '.data.uuid')
```

The idempotency key lets you repeat this write for 24 hours without creating a second
document. Use a new key for a different document. Reusing this key with different
document data returns HTTP 409. The request `id` only matches replies to requests; it
does not prevent duplicate writes.

## Read it back

```sh
curl --fail-with-body -sS -G "$base/spa" \
  --data-urlencode 'event=get_document' \
  --data-urlencode 'id=Read1' \
  --data-urlencode "document=$document_id" | jq
```

The response includes your content, the author's account ID, a signature, and a string
`seq`. This read does not require a token.

To list the collection's documents:

```sh
curl --fail-with-body -sS -G "$base/spa" \
  --data-urlencode 'event=list_documents_content' \
  --data-urlencode 'id=List1' \
  --data-urlencode "collection=$collection_id" \
  --data-urlencode 'limit=10' | jq
```

Results are under `data.documents` with numeric string keys. When `data.has_more` is
`"true"`, pass `data.next` as the next request's `after`.

Try the [browser WebSocket example](websockets.md#browser-client-example) to watch new
documents arrive.
