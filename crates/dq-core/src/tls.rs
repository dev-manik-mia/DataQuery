//! The TLS implementation every connection uses, the rustls settings for each
//! SSL mode, and the checks and reads of the PEM files a connection names (CA
//! bundle, client certificate and key).

use std::sync::{Arc, LazyLock, Once};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme};

use crate::config::{SslMode, TlsFiles};
use crate::error::{DbError, DbResult, ErrorKind};

/// Makes ring the process-wide rustls provider. Drivers that build their TLS
/// settings with rustls defaults need exactly one provider, and the
/// workspace links two (one driver brings aws-lc-rs along), so the choice is
/// made explicitly, once, before the first connection.
pub fn install_crypto_provider() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

/// Largest certificate file read (a CA bundle with hundreds of roots fits easily).
const MAX_PEM_BYTES: u64 = 4 * 1024 * 1024;

fn tls_error(message: impl AsRef<str>) -> DbError {
    DbError::new(ErrorKind::Tls, message)
}

/// Reads a PEM file named in the connection.
pub fn read_pem(path: &str, what: &str) -> DbResult<Vec<u8>> {
    let len = std::fs::metadata(path).map_err(|e| tls_error(format!("Cannot read the {what} {path}: {e}")))?.len();
    if len > MAX_PEM_BYTES {
        return Err(tls_error(format!("The {what} {path} is larger than 4 MB; choose a PEM file.")));
    }
    std::fs::read(path).map_err(|e| tls_error(format!("Cannot read the {what} {path}: {e}")))
}

/// The PEM block labels in a file (`CERTIFICATE`, `PRIVATE KEY`, …), in order, and
/// whether a block uses legacy OpenSSL encryption (`Proc-Type: 4,ENCRYPTED`).
fn labels(pem: &[u8]) -> (Vec<String>, bool) {
    let text = String::from_utf8_lossy(pem);
    let found = text
        .lines()
        .filter_map(|l| l.trim().strip_prefix("-----BEGIN ").and_then(|r| r.strip_suffix("-----")).map(str::to_owned))
        .collect();
    (found, text.contains("Proc-Type: 4,ENCRYPTED"))
}

fn is_private_key(label: &str) -> bool {
    matches!(label, "PRIVATE KEY" | "RSA PRIVATE KEY" | "EC PRIVATE KEY")
}

/// Checks the PEM files before a connection uses them, so a wrong or encrypted
/// file gets a clear message instead of a driver error: the CA file holds
/// certificates, the client certificate holds a certificate, and the key (in
/// its own file or in the certificate's `.pem`) is a plain private key.
pub fn check_files(files: &TlsFiles) -> DbResult<()> {
    if let Some(ca) = &files.ca_file {
        let (found, _) = labels(&read_pem(ca, "CA file")?);
        if !found.iter().any(|l| l == "CERTIFICATE" || l == "TRUSTED CERTIFICATE") {
            return Err(tls_error(format!(
                "The CA file {ca} holds no PEM certificate (-----BEGIN CERTIFICATE-----). Choose the CA bundle .pem."
            )));
        }
    }
    let Some((cert, key)) = files.client_pair() else {
        return Ok(());
    };
    let cert_file = labels(&read_pem(cert, "client certificate")?);
    if !cert_file.0.iter().any(|l| l == "CERTIFICATE") {
        return Err(tls_error(format!("The client certificate {cert} holds no PEM certificate.")));
    }
    let (key_labels, legacy_encrypted) = if key == cert { cert_file } else { labels(&read_pem(key, "client key")?) };
    if legacy_encrypted || key_labels.iter().any(|l| l == "ENCRYPTED PRIVATE KEY") {
        return Err(tls_error(
            "The client key is encrypted. Save an unencrypted copy (openssl pkey -in key.pem -out plain.pem) and choose that.",
        ));
    }
    if !key_labels.iter().any(|l| is_private_key(l)) {
        return Err(tls_error(if key == cert {
            format!("{cert} holds no private key. Choose the client key file too.")
        } else {
            format!("The client key {key} holds no PEM private key.")
        }));
    }
    Ok(())
}

/// The client certificate chain followed by its key as one PEM, for drivers
/// that take a single identity (reqwest). `None` without a client certificate.
pub fn client_identity_pem(files: &TlsFiles) -> DbResult<Option<Vec<u8>>> {
    let Some((cert, key)) = files.client_pair() else {
        return Ok(None);
    };
    let mut pem = read_pem(cert, "client certificate")?;
    if key != cert {
        if !pem.ends_with(b"\n") {
            pem.push(b'\n');
        }
        pem.extend(read_pem(key, "client key")?);
    }
    Ok(Some(pem))
}

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

fn setup_error(e: impl std::fmt::Display) -> DbError {
    tls_error(format!("TLS setup failed: {e}"))
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

/// rustls settings for a connection's SSL mode and certificate files. Certificates
/// are verified unless the user explicitly chose `require` or `prefer`; nothing is
/// bypassed silently.
pub fn client_config(mode: SslMode, files: &TlsFiles) -> DbResult<ClientConfig> {
    let provider = PROVIDER.clone();
    let webpki =
        || WebPkiServerVerifier::builder_with_provider(roots(files)?, provider.clone()).build().map_err(setup_error);
    let verifier: Arc<dyn ServerCertVerifier> = match mode {
        SslMode::VerifyFull => webpki()?,
        SslMode::VerifyCa => Arc::new(SkipHostname(webpki()?)),
        SslMode::Disable | SslMode::Prefer | SslMode::Require => Arc::new(NoVerification(provider.clone())),
    };
    let builder = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(setup_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier);
    // One combined .pem, or a certificate and a key file.
    let config = match files.client_pair() {
        Some((cert, key)) => {
            let chain = CertificateDer::pem_file_iter(cert)
                .map_err(|e| file_error(cert, e))?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| file_error(cert, e))?;
            let key = PrivateKeyDer::from_pem_file(key).map_err(|e| file_error(key, e))?;
            builder.with_client_auth_cert(chain, key).map_err(setup_error)?
        }
        None => builder.with_no_client_auth(),
    };
    Ok(config)
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

#[cfg(test)]
mod tests {
    use super::*;

    const CERT: &str = "-----BEGIN CERTIFICATE-----\nMIIB\n-----END CERTIFICATE-----\n";
    const KEY: &str = "-----BEGIN PRIVATE KEY-----\nMIIE\n-----END PRIVATE KEY-----\n";

    fn write(dir: &tempfile::TempDir, name: &str, text: &str) -> String {
        let path = dir.path().join(name);
        std::fs::write(&path, text).unwrap();
        path.to_string_lossy().into_owned()
    }

    #[test]
    fn combined_pem_needs_no_key_file() {
        let dir = tempfile::tempdir().unwrap();
        let both = write(&dir, "client.pem", &format!("{CERT}{KEY}"));
        let files = TlsFiles { client_cert_file: Some(both.clone()), ..TlsFiles::default() };
        assert_eq!(files.client_pair(), Some((both.as_str(), both.as_str())));
        check_files(&files).unwrap();
        assert_eq!(client_identity_pem(&files).unwrap().unwrap(), format!("{CERT}{KEY}").into_bytes());
    }

    #[test]
    fn separate_files_are_joined() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write(&dir, "c.crt", CERT.trim_end());
        let key = write(&dir, "k.key", KEY);
        let files = TlsFiles { client_cert_file: Some(cert), client_key_file: Some(key), ..TlsFiles::default() };
        check_files(&files).unwrap();
        assert_eq!(client_identity_pem(&files).unwrap().unwrap(), format!("{CERT}{KEY}").into_bytes());
    }

    #[test]
    fn clear_errors() {
        let dir = tempfile::tempdir().unwrap();
        let cert_only = write(&dir, "c.pem", CERT);
        let files = TlsFiles { client_cert_file: Some(cert_only.clone()), ..TlsFiles::default() };
        assert!(check_files(&files).unwrap_err().message.contains("no private key"));
        let encrypted =
            write(&dir, "e.pem", "-----BEGIN ENCRYPTED PRIVATE KEY-----\nx\n-----END ENCRYPTED PRIVATE KEY-----\n");
        let files =
            TlsFiles { client_cert_file: Some(cert_only), client_key_file: Some(encrypted), ..TlsFiles::default() };
        assert!(check_files(&files).unwrap_err().message.contains("encrypted"));
        let not_ca = write(&dir, "ca.pem", KEY);
        let files = TlsFiles { ca_file: Some(not_ca), ..TlsFiles::default() };
        assert!(check_files(&files).unwrap_err().message.contains("no PEM certificate"));
        let files = TlsFiles { client_key_file: Some("/k.pem".into()), ..TlsFiles::default() };
        assert!(files.validate().is_err());
    }

    #[test]
    fn combined_pem_parses_like_the_drivers() {
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let path = format!("{}/src/testdata/client_combined.pem", env!("CARGO_MANIFEST_DIR"));
        let files = TlsFiles { client_cert_file: Some(path.clone()), ..TlsFiles::default() };
        check_files(&files).unwrap();
        let (cert, key) = files.client_pair().unwrap();
        assert_eq!(CertificateDer::pem_file_iter(cert).unwrap().count(), 1);
        assert!(PrivateKeyDer::from_pem_file(key).is_ok());
    }
}
