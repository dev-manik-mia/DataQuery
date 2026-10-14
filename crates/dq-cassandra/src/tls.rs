//! TLS for Cassandra and ScyllaDB connections (same rules as `dq-postgres`). Certificates are verified unless the user
//! explicitly chose `require` or `prefer`; nothing is bypassed silently.

use std::sync::{Arc, LazyLock};

use dq_core::config::{SslMode, TlsFiles};
use dq_core::{DbError, DbResult, ErrorKind};
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

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

fn tls_error(e: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorKind::Tls, format!("TLS setup failed: {e}"))
}

fn file_error(path: &str, e: impl std::fmt::Display) -> DbError {
    DbError::new(ErrorKind::Tls, format!("Cannot read {path}: {e}"))
        .with_action(Some("Check that the certificate files exist and are PEM encoded."))
}

/// Trusted roots: the CA file when one is configured, else the platform store.
fn roots(files: &TlsFiles) -> DbResult<Arc<RootCertStore>> {
    let Some(path) = &files.ca_file else {
        return Ok(ROOTS.clone());
    };
    let mut store = RootCertStore::empty();
    for cert in CertificateDer::pem_file_iter(path).map_err(|e| file_error(path, e))? {
        store.add(cert.map_err(|e| file_error(path, e))?).map_err(|e| file_error(path, e))?;
    }
    if store.is_empty() {
        return Err(file_error(path, "no certificates found"));
    }
    Ok(Arc::new(store))
}

/// Client configuration for `mode`; `None` when TLS is off.
pub fn client_config(mode: SslMode, files: &TlsFiles) -> DbResult<Option<Arc<ClientConfig>>> {
    if mode == SslMode::Disable {
        return Ok(None);
    }
    let provider = PROVIDER.clone();
    let webpki =
        || WebPkiServerVerifier::builder_with_provider(roots(files)?, provider.clone()).build().map_err(tls_error);
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        SslMode::VerifyFull => webpki()?,
        SslMode::VerifyCa => Arc::new(SkipHostname(webpki()?)),
        SslMode::Disable | SslMode::Prefer | SslMode::Require => Arc::new(NoVerification(provider.clone())),
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(tls_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    let config = match (&files.client_cert_file, &files.client_key_file) {
        (Some(cert), Some(key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .map_err(|e| file_error(cert, e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| file_error(cert, e))?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|e| file_error(key, e))?;
            builder.with_client_auth_cert(chain, key).map_err(tls_error)?
        }
        _ => builder.with_no_client_auth(),
    };
    Ok(Some(Arc::new(config)))
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
