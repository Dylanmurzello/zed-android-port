# SSL_CERT_FILE / CURL_CA_BUNDLE env

**Status:** Removed.

Zed-the-Rust-process no longer touches these env vars at boot. All in-
process HTTPS uses `http_client_tls::tls_config()`, which builds a rustls
`ClientConfig` against the Mozilla root bundle shipped by the
`webpki-roots` crate. No env-var indirection, no platform JNI bridge,
no CA-bundle file to ship.

We previously used `rustls-platform-verifier` against Android's system
trust store via a bundled `.aar`. On Android that path was failing
HTTPS to CDN hosts (`release-assets.githubusercontent.com`, Let's Encrypt
chain, no OCSP responder) with `CertificateError::Revoked`. The Android
TrustManager bridge maps `UNDETERMINED_REVOCATION_STATUS` to "Revoked"
without a soft-fail knob, so any LE-issued cert downloaded via the
in-process HTTP client would intermittently fail.

`webpki-roots` is the same trust source `reqwest`'s default
`rustls-tls-webpki-roots` feature ships with, and it does no revocation
checking. Trade-off: the root list is a static snapshot of Mozilla NSS,
not the Android system store, so CA additions / removals require a
crate bump rather than an OS update. For Zed's use cases (extension
registry, telemetry, LSP-binary downloads from GitHub releases) this is
the right trade.

Subprocess CA story is adapter-owned:

- **Chroot adapter** : `sanitize_env_for_chroot` excludes the var.
  The rootfs ships `/etc/ssl/certs` (Debian-style), self-sufficient.
- **Bootstrap adapter**: publishes the prefix's CA bundle through
  `env_for_zed_process`; subprocesses inherit that environment. Before
  0.9.19 only the terminal got it, so Codex in the agent panel failed
  every request ("workspace routing discovery failed").
