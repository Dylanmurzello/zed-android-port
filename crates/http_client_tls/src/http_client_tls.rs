use std::sync::OnceLock;

use rustls::ClientConfig;
#[cfg(not(target_os = "android"))]
use rustls_platform_verifier::ConfigVerifierExt;

static TLS_CONFIG: OnceLock<rustls::ClientConfig> = OnceLock::new();

pub fn tls_config() -> ClientConfig {
    TLS_CONFIG
        .get_or_init(|| {
            // rustls uses the `aws_lc_rs` provider by default
            // This only errors if the default provider has already
            // been installed. We can ignore this `Result`.
            rustls::crypto::aws_lc_rs::default_provider()
                .install_default()
                .ok();

            // On Android the platform verifier needs a JNI context Zed doesn't
            // set up, and it reports certificates without an OCSP responder,
            // such as Let's Encrypt's, as revoked.
            #[cfg(target_os = "android")]
            return ClientConfig::builder()
                .with_root_certificates(rustls::RootCertStore {
                    roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                })
                .with_no_client_auth();

            #[cfg(not(target_os = "android"))]
            ClientConfig::with_platform_verifier().unwrap_or_else(|error| {
                log::error!(
                    "failed to load platform TLS certificate verifier, falling back to bundled webpki roots: {error}"
                );
                ClientConfig::builder()
                    .with_root_certificates(rustls::RootCertStore {
                        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
                    })
                    .with_no_client_auth()
            })
        })
        .clone()
}
