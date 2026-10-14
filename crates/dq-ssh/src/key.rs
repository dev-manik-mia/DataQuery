//! SSH private keys from files: OpenSSH keys and the `.pem` keys clouds hand
//! out (AWS EC2 `-----BEGIN RSA PRIVATE KEY-----`, PKCS#8, SEC1 EC keys).
//!
//! The text is normalised first (byte-order mark, Windows line ends,
//! indentation, extra blocks such as certificates or EC parameters), and
//! formats that cannot be used get a message that says what to do instead.

use russh::keys::PrivateKey;

/// Largest key file read; real keys are a few kilobytes.
const MAX_KEY_BYTES: u64 = 256 * 1024;

const PRIVATE_KEY_LABELS: &[&str] =
    &["OPENSSH PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY", "PRIVATE KEY", "ENCRYPTED PRIVATE KEY"];

/// Why a key file could not be used. `Encrypted` asks for a passphrase.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum KeyError {
    Encrypted,
    Unusable(String),
}

/// Loads the private key at `path`, decrypting it with `passphrase` when it is encrypted.
pub(crate) fn load_private_key(path: &str, passphrase: Option<&str>) -> Result<PrivateKey, KeyError> {
    let unusable = |m: String| KeyError::Unusable(m);
    let len = std::fs::metadata(path).map_err(|e| unusable(format!("Cannot read SSH key {path}: {e}")))?.len();
    if len > MAX_KEY_BYTES {
        return Err(unusable(format!("{path} is too large to be an SSH private key.")));
    }
    let bytes = std::fs::read(path).map_err(|e| unusable(format!("Cannot read SSH key {path}: {e}")))?;
    let text = String::from_utf8(bytes).map_err(|_| {
        unusable(format!("{path} is not a text key file. Choose an OpenSSH or PEM (.pem) private key."))
    })?;
    let block = private_key_block(&text).map_err(|m| unusable(format!("{path}: {m}")))?;
    let encrypted = block.label == "ENCRYPTED PRIVATE KEY" || block.text.contains("Proc-Type: 4,ENCRYPTED") || {
        // OpenSSH keys say so inside the base64 body; only a decode attempt tells.
        block.label == "OPENSSH PRIVATE KEY"
            && matches!(russh::keys::decode_secret_key(&block.text, None), Err(russh::keys::Error::KeyIsEncrypted))
    };
    if block.text.contains("Proc-Type: 4,ENCRYPTED") && !block.text.contains("DEK-Info: AES-128-CBC") {
        return Err(unusable(format!(
            "{path} uses old OpenSSL key encryption (e.g. DES-EDE3-CBC), which is not supported. Re-save it with \
             `ssh-keygen -p -f {path}` (keeps the passphrase, writes the OpenSSH format)."
        )));
    }
    // A passphrase saved for an unencrypted key is ignored: it would make the key unreadable.
    let passphrase = if encrypted { passphrase.filter(|p| !p.is_empty()) } else { None };
    if encrypted && passphrase.is_none() {
        return Err(KeyError::Encrypted);
    }
    russh::keys::decode_secret_key(&block.text, passphrase).map_err(|e| match e {
        russh::keys::Error::KeyIsEncrypted => KeyError::Encrypted,
        _ if encrypted => unusable(format!("Cannot decrypt SSH key {path}: check its passphrase.")),
        other => unusable(format!("Cannot read SSH key {path}: {other}")),
    })
}

/// The first private-key PEM block, normalised.
struct Block {
    label: &'static str,
    text: String,
}

fn private_key_block(text: &str) -> Result<Block, String> {
    let text = text.trim_start_matches('\u{feff}');
    if text.trim_start().starts_with("PuTTY-User-Key-File") {
        return Err("this is a PuTTY .ppk key. In PuTTYgen choose Conversions → Export OpenSSH key, then choose \
                    that file."
            .into());
    }
    let lines: Vec<&str> = text.lines().map(str::trim).collect();
    let begin = lines.iter().enumerate().find_map(|(i, l)| {
        let label = l.strip_prefix("-----BEGIN ")?.strip_suffix("-----")?;
        PRIVATE_KEY_LABELS.iter().find(|x| **x == label).map(|x| (i, *x))
    });
    let Some((start, label)) = begin else {
        let first = lines.iter().find(|l| !l.is_empty()).copied().unwrap_or("");
        return Err(if first.starts_with("ssh-") || first.starts_with("ecdsa-") || first.contains("PUBLIC KEY") {
            "this is a public key. Choose the private key (the file without .pub).".into()
        } else if first.contains("CERTIFICATE") {
            "this file holds a certificate, not a private key.".into()
        } else {
            "no private key found. Choose an OpenSSH or PEM (.pem) private key file.".into()
        });
    };
    let end_marker = format!("-----END {label}-----");
    let end = lines[start..]
        .iter()
        .position(|l| *l == end_marker)
        .map(|i| start + i)
        .ok_or_else(|| format!("the {label} block is not closed ({end_marker} is missing)."))?;
    let mut block = lines[start..=end].join("\n");
    block.push('\n');
    Ok(Block { label, text: block })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str) -> String {
        format!("{}/src/testdata/{name}", env!("CARGO_MANIFEST_DIR"))
    }

    fn load(name: &str, passphrase: Option<&str>) -> Result<PrivateKey, KeyError> {
        load_private_key(&data(name), passphrase)
    }

    #[test]
    fn reads_cloud_pem_keys() {
        for name in ["rsa_pkcs1.pem", "rsa_pkcs8.pem", "ec_sec1.pem", "ed25519_openssh.pem", "rsa_bom_crlf.pem"] {
            assert!(load(name, None).is_ok(), "{name}: {:?}", load(name, None).err());
        }
        // A passphrase saved for an unencrypted key does no harm.
        assert!(load("rsa_pkcs1.pem", Some("unused")).is_ok());
        assert_eq!(
            load("rsa_pkcs1.pem", None).unwrap().algorithm(),
            load("rsa_bom_crlf.pem", None).unwrap().algorithm()
        );
    }

    #[test]
    fn encrypted_keys_need_their_passphrase() {
        for name in ["rsa_pkcs8_encrypted.pem", "rsa_aes128.pem"] {
            assert_eq!(load(name, None).err(), Some(KeyError::Encrypted), "{name}");
            assert!(load(name, Some("secret")).is_ok(), "{name}");
            assert!(
                matches!(load(name, Some("wrong")), Err(KeyError::Unusable(m)) if m.contains("passphrase")),
                "{name}"
            );
        }
    }

    #[test]
    fn unusable_files_say_why() {
        let message = |name: &str| match load(name, Some("secret")) {
            Err(KeyError::Unusable(m)) => m,
            other => panic!("{name}: {other:?}"),
        };
        assert!(message("rsa_des3.pem").contains("ssh-keygen -p"));
        assert!(message("putty.ppk").contains("PuTTYgen"));
        assert!(message("ed25519.pub").contains("public key"));
        assert!(load("missing.pem", None).is_err());
    }
}
