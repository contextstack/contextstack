//! HTTP client TLS configuration shared by the gateway and harness.

/// Build a reqwest client with the bundled and native trust roots.
pub fn builder() -> reqwest::ClientBuilder {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    // Preserve reqwest 0.12's native-root loading. SSL_CERT_FILE and SSL_CERT_DIR
    // override the native store while leaving the bundled roots available.
    roots.add_parsable_certificates(rustls_native_certs::load_native_certs().certs);
    let mut tls = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    reqwest::Client::builder().tls_backend_preconfigured(tls)
}
