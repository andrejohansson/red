# Security Policy

## Reporting a vulnerability

Open a [private security advisory](https://github.com/vojir-mikulas/red/security/advisories/new)
on GitHub, or email <vojir.mikulas@gmail.com>. Please include what you did, what
happened, and what you expected; a proof of concept helps but is not required.

You can expect an acknowledgement within a few days and an assessment within two
weeks. Please do not disclose publicly before a fix is released.

## Supported versions

Only the latest release is supported. Fixes land on `main` and ship in the next
release rather than being backported.

## What RED protects

RED is a desktop database client, so its threat model is mostly about the
credentials it holds and the statements it runs on your behalf.

- **Credentials.** Database, SSH, proxy and AI passwords live in the operating
  system keychain, keyed by connection id. They are never written to
  `connections.toml`; the shape that file is serialized from has no password
  field at all. Every file RED writes under the config directory is created
  `0600` on Unix.
- **Destructive statements.** Statements are graded before they run and the
  riskiest need a typed confirmation. A connection can be marked read-only, and a
  deployment marker (Local/Dev/Staging/Prod) scales how much RED asks.
- **The AI assistant.** Off unless configured, and gated by an access tier
  (`off`/`schema`/`read`/`write`) that fails closed: a tool nobody has classified
  is treated as a write. Every write is refused or shown to you for an explicit
  approval before it runs; no tool writes without one. The read tier vets each
  statement rather than trusting the model's own account of it.
- **Transport.** TLS is available for every engine, host keys are verified with a
  trust-on-first-use prompt, and RED never disables certificate verification.

## Update trust model

RED checks GitHub for a newer release every six hours (turn it off with
`auto_update = false` under `[update]` in `settings.toml`). An update is
installed only if it passes every check below, and is refused otherwise:

- The download host is pinned to `github.com`, over HTTPS.
- The version must be strictly newer; a downgrade is refused.
- **macOS:** the bundle's signature is verified (`codesign --verify --deep
  --strict`), Gatekeeper is asked to assess it (`spctl --assess`), and its Team
  ID must match the installed app's. A Team ID that cannot be read is a refusal,
  not a pass.
- **Linux and Windows:** the artefact is checked against the `.sha256` sidecar
  published with the release. These builds are not OS-signed.
- The new version is staged in a `0700` temporary directory and swapped in
  atomically, so a failure never leaves a half-written install.

## Known limitations

- The Linux AppImage and the Windows executable are unsigned; the checksum
  sidecar is what integrity there is.
- Imported connections from other tools (DBeaver, DBGate, DataGrip,
  RedisInsight) are decrypted with those tools' own well-known keys. That is what
  makes the import possible, and it is not a secret RED can keep.
