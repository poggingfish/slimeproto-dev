# Slimeproto

Slimeproto is a server for accounts, collections, and signed documents. Clients write
and read data over HTTP, then use WebSocket subscriptions to hear about new records.

An account has an Ed25519 keypair and an ID derived from its public key. You log in with
that account ID and a password. There are no usernames on this server. Friendly names
and finding the server that hosts an account are left to an external service.

Collections group documents under a name, such as `notes`. A document has an
application-defined type, some content, and optional references to other documents. The
current API creates records and reads them back. It has no update or delete actions for
collections or documents.

## A small example

A notes client might create a listed collection called `notes`, then write:

```json
{
  "event": "create_document",
  "id": "Note1",
  "data": {
    "collection": "notes",
    "type": "note",
    "content": {
      "title": "Shopping list",
      "text": "Coffee, bread, tomatoes"
    }
  }
}
```

The server returns a document UUID and a sequence number. A subscriber gets a creation
event with metadata and can fetch the content using that UUID.

Document content and other action data use strings and nested objects. Write `"true"`
instead of `true`, and represent lists as objects with keys like `"0"` and `"1"`.

## Listed and unlisted data

Listed collections appear in public discovery, user subscriptions, and the firehose.
Their documents are available through sequence catch-up.

Unlisted collections stay out of discovery. Anyone who knows their UUID can read them or
subscribe with a valid account token. Document UUIDs also allow public reads, so take
care when sharing them. New references can only target documents in listed collections.
Unlisted documents can reference listed documents without appearing in reverse lookups.

## Stored data and live delivery

Collections and documents persist across restarts. WebSocket events and direct messages
are transient. Clients can recover missed listed documents using [sequence
catch-up](reading.md#sequence-catch-up).

The server holds each account's signing key and signs records on its behalf. Data is not
end-to-end encrypted.
