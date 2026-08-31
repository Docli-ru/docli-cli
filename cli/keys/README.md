# Release signing key (v0.28.0 D9)

`release.pub.b64` holds the base64 BODY of the minisign public key pinned into every `docli`
binary. An empty pin makes `docli self-update` refuse (never verify-nothing).

**MINTED + ESCROWED 2026-08-30** (`RWQuy3bXJZ0+…`): Bitwarden item «docli-release.key
(minisign)» — the full secret-key file content and its password live in HIDDEN FIELDS (the
account has no premium, so no file attachments; the fields ARE the escrow). Escrow proven by a
full restore roundtrip: key + password pulled back from the vault, a probe manifest signed and
verified against the repo-pinned public body. The local `~/docli-release-key/` originals were
deleted after the proof — the vault copy is now the ONLY secret-key copy. To sign a release:
save the key field to `docli-release.key`, run `minisign -S -s docli-release.key -m
manifest.json`, enter the password field when prompted.

Original runbook (for a future re-mint/rotation):

1. On the operator machine (never in-cluster): `minisign -G -p docli-release.pub -s docli-release.key`
2. Escrow `docli-release.key` + its password in Bitwarden (the KEK precedent — the key never
   touches the server keyring or CI).
3. Paste the SECOND line of `docli-release.pub` (the base64 body) into `release.pub.b64`.
4. Commit; every release then signs `manifest.json` with
   `minisign -S -s docli-release.key -m manifest.json` (see `/release-cli`).
