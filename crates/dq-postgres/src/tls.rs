//! TLS for PostgreSQL connections. Certificates are verified unless the user
//! explicitly chose `require` or `prefer`; nothing is bypassed silently.

use std::sync::{Arc, LazyLock};

use dq_core::config::SslMode;
use dq_core::{DbError, DbResult, ErrorKind};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};
use tokio_postgres_rustls::MakeRustlsConnect;

static PROVIDER: LazyLock<Arc<CryptoProvider>> = LazyLock::new(|| Arc::new(rustls::crypto::ring::default_provider()));

/// Platform trust store, falling back to the Mozilla roots when it is empty.
static ROOTS: LazyLock<Arc<RootCertStore>> = LazyLock::new(|| {
    let mut roots = RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    let (added, _) = roots.add_parsable_certificates(native.certs);
    if added == 0 {
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    }
    Arc::new(roots)
});

pub fn connector(mode: SslMode) -> DbResult<MakeRustlsConnect> {
    let provider = PROVIDER.clone();
    let tls_error = |e: rustls::Error| DbError::new(ErrorKind::Tls, format!("TLS setup failed: {e}"));
    let webpki = || {
        WebPkiServerVerifier::builder_with_provider(ROOTS.clone(), provider.clone())
            .build()
            .map_err(|e| DbError::new(ErrorKind::Tls, format!("TLS setup failed: {e}")))
    };
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        SslMode::VerifyFull => webpki()?,
        SslMode::VerifyCa => Arc::new(SkipHostname(webpki()?)),
        SslMode::Disable | SslMode::Prefer | SslMode::Require => Arc::new(NoVerification(provider.clone())),
    };
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    Ok(MakeRustlsConnect::new(config))
}

/// `verify-ca`: the chain must be trusted; the host name is not checked.
#[derive(Debug)]
struct SkipHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for SkipHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self.0.verify_server_cert(end_entity, intermediates, server_name, ocsp, now) {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(m, c, d)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(m, c, d)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}

/// `require`/`prefer`: encrypted, but the certificate is not verified. Only
/// used when the user picked one of those modes.
#[derive(Debug)]
struct NoVerification(Arc<CryptoProvider>);

impl ServerCertVerifier for NoVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        m: &[u8],
        c: &CertificateDer<'_>,
        d: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(m, c, d, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
