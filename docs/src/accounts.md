# Accounts and login

Log in with an account ID and password. Accounts have no usernames; friendly names and
server discovery belong to a separate naming service.

Registration creates an Ed25519 keypair held by the server. The account ID comes from
its public key. You can look up that key with `get_identity`.

## POST /register

Passwords must contain at least 10 characters and no more than 1024 UTF-8 bytes. Each
registration creates a new account, even if you reuse a password. Unknown fields,
including `username`, are rejected.

```sh
curl http://localhost:3000/register \
  -H 'Content-Type: application/json' \
  -d '{"password":"example-password"}'
```

For an invite-only server, send
`{"password":"example-password","registration_key":"the-server-invite"}` instead.
Missing or incorrect keys return HTTP 403. The same `registration_key` field is accepted
by `/identity/import`.

Success: HTTP 201. Save the returned account ID for login:

```json
{"account_id":"<account_id>","algorithm":"ed25519","public_key":"<64 lowercase hex characters>"}
```

## POST /token

Log in with the account ID and password:

```sh
curl http://localhost:3000/token \
  -H 'Content-Type: application/json' \
  -d '{"account_id":"<account_id>","password":"example-password"}'
```

Success: HTTP 200. Tokens last 24 hours; `expires_at` is a Unix timestamp in seconds.

```json
{"token":"<token>","expires_at":1791676800}
```

Registration, login, and identity import/export errors use `{"fakywaky":"<error>"}`
without an event envelope.

## Identity export and import

Export requires a valid bearer token, reentry of the current account password, and a
backup password. Both new account passwords and backup passwords follow registration's
length limits. The backup password may differ from the login password.

```sh
curl http://localhost:3000/identity/export \
  -H 'Content-Type: application/json' \
  -H 'Authorization: Bearer <token>' \
  -d '{"password":"example-password","backup_password":"separate-backup-password"}'
```

Success: HTTP 200 with an encrypted backup containing `format`, `account_id`,
`public_key`, `kdf`, `cipher`, `salt`, `nonce`, and `ciphertext`. Store that response as
a file. Export never returns a plaintext private key. Salt, nonce, public key, and
ciphertext use lowercase hexadecimal.

To restore onto an independent server, POST `/identity/import`:

```json
{
  "backup": {"format":"slimeproto-identity-v1","account_id":"...","public_key":"...","kdf":"argon2id-v19-m19456-t2-p1","cipher":"xchacha20poly1305","salt":"...","nonce":"...","ciphertext":"..."},
  "backup_password":"separate-backup-password",
  "password":"new-login-password"
}
```

A successful import returns HTTP 201 with the same public identity fields as
registration. It preserves your account ID and signing key, but sets a new login
password on the destination server. Log in there to get a token.

An account already present on the destination returns HTTP 409; its password and key
remain unchanged. A wrong backup password or modified backup returns HTTP
400. All import and export responses carry `Cache-Control: no-store`.

Backups contain identity keys only. Collections, documents, messages, and tokens are
excluded. Importing leaves the old server's key in place and does not change
naming-service records. The server's own copy of the signing key is stored without
backup-password encryption.

The [backup format](identity.md#encrypted-backups) documents the encryption and field
encodings.

## Migration and operational notes

### Migrating older databases

The default database directory is `data/slimeproto-identities-v1`. Use
`SLIMEPROTO_DATA_PATH` to choose another location.

Older `data/slimeproto` databases are left untouched. They are not converted to the
current identity schema, and selecting one causes startup to fail. Register new accounts
when moving from that schema; old account IDs and tokens do not transfer. Other nonempty
databases without the current identity schema marker also fail startup.

### Password and backup request limits

Registration, login, import, and export share a limit of four concurrent password or
backup operations. When they are all busy, another request returns HTTP 503 with
`Retry-After: 1` and `Cache-Control: no-store`. Wait one second before retrying.
Cancelling a request does not free its slot until the operation finishes.

### Session cleanup

Expired sessions are removed at startup and every 60 seconds in small batches. Cleanup
does not extend a token's lifetime. Session indexes are upgraded automatically without
invalidating live tokens. Revoking a token removes its session and expiry entry
together.
