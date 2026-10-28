# SSL_CERT_FILE / CURL_CA_BUNDLE env

**Status:** Active.

Android has none of the CA bundle paths Linux programs look for. The
Bootstrap adapter sets `SSL_CERT_FILE` and `CURL_CA_BUNDLE` to the
bootstrap's `$PREFIX/etc/tls/cert.pem` in the app's environment
(`env_for_zed_process`), so every program it starts gets them: the
terminal, language servers, and agents such as Codex, whose requests all
failed with "workspace routing discovery failed" before 0.9.19, when only
the terminal had them. The chroot's programs use its `/etc/ssl/certs`.

Zed's own HTTPS doesn't use them: `http_client_tls::tls_config()` builds a
rustls `ClientConfig` against the Mozilla root bundle shipped by the
`webpki-roots` crate.

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
