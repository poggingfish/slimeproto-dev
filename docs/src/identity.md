# Identity formats

Use these byte formats to derive account IDs, verify record signatures, or read
encrypted identity backups.

## Account IDs

Each account starts with a random 32-byte Ed25519 signing seed. Its public key is 32
bytes, returned as 64 lowercase hexadecimal characters.

The server derives the account ID as follows:

1. Hash the bytes `slimeproto:account:v1:` followed by the raw public key with SHA-256.
2. Take the first 16 hash bytes.
3. Set byte 6 to `(byte6 & 0x0f) | 0x80` for UUID version 8.
4. Set byte 8 to `(byte8 & 0x3f) | 0x80` for the UUID variant.
5. Encode as lowercase hex, grouped into lengths `12:4:4:4:4:4`.

The result is a 37-character account ID. Collection and document UUIDs use the same
textual grouping but are generated as UUIDv7 values.

You can check the derivation with Python's standard library:

```python
import hashlib

def account_id(public_key_hex):
    public_key = bytes.fromhex(public_key_hex)
    if len(public_key) != 32:
        raise ValueError("expected a 32-byte public key")
    raw = bytearray(hashlib.sha256(
        b"slimeproto:account:v1:" + public_key
    ).digest()[:16])
    raw[6] = (raw[6] & 0x0f) | 0x80
    raw[8] = (raw[8] & 0x3f) | 0x80
    encoded = raw.hex()
    return ":".join(encoded[a:b] for a, b in (
        (0, 12), (12, 16), (16, 20), (20, 24), (24, 28), (28, 32)
    ))
```

An imported seed keeps the same account ID. Keys cannot be rotated. Losing every copy of
the key loses the identity; a compromised key permanently compromises it.

## Signed records

The server signs stored collection and document records with the author's Ed25519 key.
Signatures are 64 bytes, encoded as 128 lowercase hex characters.

The signed message is a domain prefix followed by compact UTF-8 JSON. Remove the
top-level `signature` and `seq` fields. Sort object keys recursively in lexicographic
order by their UTF-8 bytes, preserving array order.

| Record | Domain prefix |
| --- | --- |
| Collection | `slimeproto:collection:v1:` followed by a zero byte |
| Document | `slimeproto:document:v1:` followed by a zero byte |

The stored record format differs from the action response format. Reconstruct these
fields before verifying:

| Collection field | Signed JSON type |
| --- | --- |
| `uuid`, `name`, `author` | String |
| `listed` | Boolean |

| Document field | Signed JSON type |
| --- | --- |
| `uuid`, `collection`, `author`, `type` | String |
| `name` | String when present, otherwise JSON `null` |
| `content` | Object containing strings and nested objects |
| `refs` | Array of UUID strings, preserving reference order |

A collection's signed JSON looks like this:

```json
{"author":"<account_id>","listed":true,"name":"notes","uuid":"<collection_uuid>"}
```

To verify a document, turn the API's numeric-key `refs` object into an array, insert
`name: null` if it was omitted, and leave out `seq`, `signature`, and `collection_name`.
The sequence is local server ordering and is not signed. WebSocket creation metadata
does not include document content, so fetch the full record first.

Fetch the public key with `get_identity`, check that it derives to the expected account
ID, then verify the signature over the prefix and serialized bytes. Match JSON escaping
as well as key ordering: leave non-ASCII text as UTF-8, escape quotes and backslashes,
and use the short escapes for backspace, tab, newline, form feed, and carriage return.
Encode other control characters below U+0020 as lowercase `\u00xx` escapes. Do not
escape forward slashes.

## Encrypted backups

An exported backup is a JSON object with these fields:

| Field | Value |
| --- | --- |
| `format` | `slimeproto-identity-v1` |
| `account_id` | Derived account ID |
| `public_key` | 32-byte public key in lowercase hex |
| `kdf` | `argon2id-v19-m19456-t2-p1` |
| `cipher` | `xchacha20poly1305` |
| `salt` | 16 random bytes in lowercase hex |
| `nonce` | 24 random bytes in lowercase hex |
| `ciphertext` | 48 bytes in lowercase hex, including the authentication tag |

The backup password and salt derive a 32-byte encryption key using Argon2id version 19,
with 19456 KiB of memory, two iterations, and parallelism one. XChaCha20-Poly1305
encrypts the 32-byte signing seed with that key and nonce. The associated data is the
UTF-8 encoding of:

```text
slimeproto:identity-backup:v1:<account_id>:<public_key>
```

On import, the server decrypts the seed and checks that both the derived account ID and
public key match the backup. A new export gets a new salt and nonce.

The running server stores the seed in its database without backup-password encryption.
Protect the database as well as exported backups. The server can sign on behalf of any
account it hosts.
