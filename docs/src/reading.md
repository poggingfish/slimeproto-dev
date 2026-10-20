# GET /spa actions

Include `event=<action>&id=<request_id>` with every query. Bracketed fields are
optional. `user` takes an account ID, `collection` and `document` take UUIDs, and `name`
takes a collection name.

| Action | Query fields | Response `data` |
| --- | --- | --- |
| `get_identity` | `user=<account_id>` | `account_id`, `algorithm` (`ed25519`), and hex `public_key` |
| `get_storage_usage` | `user=<account_id>` | String `used_bytes` and `max_bytes`; authenticated owner only |
| `get_collection_uuid` | `user=<account_id>&name=notes` | `{"uuid":"<collection_uuid>"}`; listed collections only |
| `get_collection` | `collection=<collection_uuid>` | `uuid`, `name`, `author` account ID, `listed` string, `signature` |
| `get_last_document` | `collection=<collection_uuid>` | `{"uuid":"<last_document_uuid>"}`; empty string for an empty collection |
| `get_document_content` | `document=<document_uuid>` | `{"content":{...}}` |
| `get_document` | `document=<document_uuid>` | `uuid`, string `seq`, `collection` UUID, `author` account ID, `signature`, `type`, optional `name`, `content`, and `refs` |
| `get_documents` | `documents=<uuid1>,<uuid2>,...` | `{"documents":{"0":{...},"1":{...}}}`; full document records in requested order |
| `get_document_refs` | `document=<document_uuid>` | `{"refs":{"0":"<target_uuid>","1":"<target_uuid>"}}`; no pagination |
| `list_document_referrers` | `document=<target_uuid>[&after=<source_uuid>&limit=5000]` | Listed source document UUID page, oldest first |
| `list_document_referrers_content` | `document=<target_uuid>[&after=<source_uuid>&limit=500]` | Listed source document records with content and refs, oldest first |
| `list_documents` | `collection=<collection_uuid>[&after=<document_uuid>&limit=5000]` | Document UUID page, oldest first |
| `list_documents_reverse` | `collection=<collection_uuid>[&after=<document_uuid>&limit=5000]` | Document UUID page, newest first |
| `list_documents_content` | `collection=<collection_uuid>[&after=<document_uuid>&limit=500]` | Document records with content, oldest first |
| `list_user_documents` | `user=<account_id>[&after=<document_uuid>&limit=5000]` | User's listed document UUID page, oldest first |
| `list_collections` | `user=<account_id>[&after=<collection_uuid>&limit=5000]` | User's listed collection metadata page |
| `list_all_collections` | `user=<account_id>[&after=<collection_uuid>&limit=5000]` | All of the user's collection metadata, including unlisted |
| `catch_up_documents` | `[user=<account_id>&after_seq=0&upto_seq=<snapshot>&limit=500]` | Listed document records with sequence-based continuation |

`list_all_collections` and `get_storage_usage` require a bearer token belonging to the
requested user. All other GET actions are public. Providing a token to public discovery
actions does not reveal unlisted collections/documents.

Reads using a known collection or document UUID work for unlisted data.
`get_collection_uuid` hides unlisted collections even from the owner; use
`list_all_collections` to discover your own unlisted collections.

`get_documents` accepts 1-500 distinct document UUIDs separated by commas without
spaces. They can belong to different collections. Results use numeric keys in request
order and come from one database snapshot.

Invalid or duplicate UUIDs, or more than 500, return HTTP 400. If any document is
missing, the whole request returns HTTP 404.

```text
GET /spa?event=get_document&id=Read1&document=<document_uuid>
GET /spa?event=get_documents&id=Read2&documents=<uuid1>,<uuid2>
```

Use URL encoding for query values, particularly names with spaces, `+`, or `&`:

```sh
curl -G http://localhost:3000/spa \
  --data-urlencode 'event=get_collection_uuid' \
  --data-urlencode 'id=Lookup1' \
  --data-urlencode 'user=<account_id>' \
  --data-urlencode 'name=my notes'
```

For the private listing, add `-H 'Authorization: Bearer <token>'` to a GET request with
`event=list_all_collections` and `user=<account_id>`.

## Full document response

`GET /spa?event=get_document&id=Read1&document=<document_uuid>` returns HTTP 200 with
the full record. IDs below are placeholders; `<signature>` stands for 128 lowercase
hexadecimal characters.

```json
{
  "event": "get_document_ok",
  "id": "Read1",
  "data": {
    "uuid": "<document_uuid>",
    "seq": "124",
    "collection": "<collection_uuid>",
    "author": "<account_id>",
    "signature": "<signature>",
    "type": "note",
    "name": "coffee",
    "content": {
      "text": "Remember the coffee",
      "details": {
        "shop": "Corner store"
      }
    },
    "refs": {
      "0": "<referenced_document_uuid>"
    }
  }
}
```

`name` is omitted for unnamed documents. A document without references has `"refs": {}`.
Sequence numbers are strings, and content contains only strings and nested objects.

## UUID pagination

ID and collection pages default to a maximum of 5000 entries. Content pages default to a
maximum of 500. `limit` must be a positive integer within the corresponding maximum.

Lists are objects indexed by numeric string keys, not arrays. Interpret keys
numerically; JSON property order is not guaranteed.

```json
{
  "event":"list_documents_ok",
  "id":"List1",
  "data":{
    "documents":{"0":"<first_uuid>","1":"<second_uuid>"},
    "next":"<second_uuid>",
    "has_more":"true"
  }
}
```

If `has_more` is `"true"`, pass `after=<next>` to continue. The cursor is excluded.
Forward document pages return newer documents; reverse pages return older ones. Document
ordering uses creation sequence. Collection ordering uses UUID order.

On the last page, `next` is `""` and `has_more` is `"false"`. An empty result uses an
empty list object. Cursors must belong to the requested collection/user and be eligible
for that listing; invalid cursors return HTTP 400.

Content pages put full records under `documents`, using the fields in the [document
example](#full-document-response). Collection pages use `collections`; each entry has
`uuid`, `name`, `author`, `signature`, and string `listed`.

## Document references

All full document responses (`get_document`, `get_documents`, content pages, and
catch-up) include `refs` as a numeric-key object. Existing documents without stored
references return `refs: {}`. Reference order is preserved. `get_document_refs` returns
only this object.

```text
GET /spa?event=get_document_refs&id=Refs1&document=<source_uuid>
GET /spa?event=list_document_referrers&id=Refs2&document=<target_uuid>&limit=100
GET /spa?event=list_document_referrers_content&id=Refs3&document=<target_uuid>&limit=100
```

Referrer queries return pages with `documents`, `next`, and `has_more`. They include
source documents from all accounts' listed collections, ordered by creation sequence.
Unlisted sources are excluded, even with an owner token. New references can only
target listed documents. You can still query a known unlisted target UUID to inspect
references created before this rule; its UUID does not allow new references to it.

An `after` cursor must identify a listed source that references this target; otherwise
it returns HTTP 400. A missing target returns HTTP 404. An existing target with no
listed referrers returns an empty page.

## Sequence catch-up

`catch_up_documents` recovers listed document creations after a numeric sequence. Omit
`user` for global/firehose recovery; include it for user recovery. `after_seq` defaults
to `0` and is exclusive. It need not identify an existing document. Unlisted creations
cause sequence gaps.

```text
/spa?event=catch_up_documents&id=Catch1&after_seq=123&limit=500
```

The response contains full document records under numeric string keys. Each record also
includes `collection_name`. For example, a final page with one matching document returns
HTTP 200 with:

```json
{
  "event": "catch_up_documents_ok",
  "id": "Catch1",
  "data": {
    "documents": {
      "0": {
        "uuid": "<document_uuid>",
        "seq": "124",
        "collection": "<collection_uuid>",
        "collection_name": "notes",
        "author": "<account_id>",
        "signature": "<signature>",
        "type": "note",
        "name": "coffee",
        "content": {
          "text": "Remember the coffee",
          "details": {
            "shop": "Corner store"
          }
        },
        "refs": {
          "0": "<referenced_document_uuid>"
        }
      }
    },
    "next_seq": "124",
    "snapshot": "124",
    "has_more": "false"
  }
}
```

If more documents remain within the snapshot, `has_more` is `"true"` and `next_seq` is
the last sequence returned on that page. An empty result uses `"documents": {}`.

For the next page, send `after_seq=<next_seq>` and `upto_seq=<snapshot>` with the same
user scope. The snapshot is an inclusive upper boundary: documents created after it wait
for the next batch.

On the final page, `next_seq` equals `snapshot`, even when no documents match. Save it
as your checkpoint. Omit `upto_seq` when starting the next batch. A checkpoint ahead of
public history returns an empty result and stays unchanged.

Catch-up returns records, not historical event envelopes or original correlation IDs. It
does not replay collection creations or direct messages.
