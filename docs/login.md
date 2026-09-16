# Login and credential storage

`wally login` is a browser device flow, the same shape `gh auth login` uses.
No password ever reaches the CLI.

## The flow

1. `wally login` calls the console to start an attempt and gets back a
   `request_code`, a `poll_secret`, and a `verification_url`.
2. wally checks that URL's origin is one it trusts, then opens it in your
   browser. You approve the sign-in there.
3. wally polls the console until the attempt is granted, then receives an
   access token, a refresh token, and your email.
4. The tokens are encrypted and written to the credential store.

`poll_secret` is what proves the process polling is the one that started the
attempt; the console only ever stores a hash of it, never the value itself.

## Where credentials live

The store tries the OS keystore first (macOS Keychain, Linux Secret
Service/libsecret, Windows via `go-keyring`'s backend) under the service name
`RunAnywhere Wally`. If the keystore call itself fails, for example no Secret
Service is running, it falls back to a local file: AES-256-GCM encrypted, with
the key held in its own `0600` file next to the ciphertext. That fallback is
weaker than an OS keystore's per-app binding, but it is real encryption, never
a bare plaintext token file.

`WALLY_PROFILE_DIR` overrides where the store looks, on disk and as the
keystore account name, which is what lets more than one wally account share a
machine.

## Logout

`wally logout` revokes the session against the console, deletes the stored
credential (keychain or file, whichever holds it), then verifies nothing is
left. A revoke that fails against the console still clears the local
credential and says so, rather than leaving a token behind because the
network call didn't succeed.

## Endpoints

Two hosts, not one deployment: the console API serves `/auth/cli/*`, `/v1/me`,
and `/v1/cli/*`; the web origin is only the page you approve the sign-in on.
Both are baked into the binary at build time (see the root `README.md` and
`scripts/build.sh` / `scripts/dev.sh`) and can be overridden at runtime with
`WALLY_CONSOLE_URL` and `WALLY_CONSOLE_WEB_URL`, which is how a local dev
console gets tested without a rebuild.
