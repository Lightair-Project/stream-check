//! The self-signed certificate RTMPS uses, made at first start and kept in the data directory.

use std::path::Path;
use std::sync::Arc;

use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::rustls::pki_types::{CertificateDer, PrivateKeyDer};

/// Loads `cert.pem` and `key.pem` from `dir`, or makes them for these names.
pub fn acceptor(dir: &Path, names: Vec<String>) -> Result<(TlsAcceptor, std::path::PathBuf), String> {
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let (cert_path, key_path) = (dir.join("cert.pem"), dir.join("key.pem"));
    if !cert_path.exists() || !key_path.exists() {
        let mut names = names;
        names.push("localhost".into());
        names.dedup();
        let ck = rcgen::generate_simple_self_signed(names).map_err(|e| e.to_string())?;
        std::fs::write(&cert_path, ck.cert.pem()).map_err(|e| e.to_string())?;
        std::fs::write(&key_path, ck.key_pair.serialize_pem()).map_err(|e| e.to_string())?;
    }
    let cert_pem = std::fs::read(&cert_path).map_err(|e| e.to_string())?;
    let key_pem = std::fs::read(&key_path).map_err(|e| e.to_string())?;
    let certs: Vec<CertificateDer<'static>> = pem_blocks(&cert_pem, "CERTIFICATE").into_iter().map(CertificateDer::from).collect();
    let key = pem_blocks(&key_pem, "PRIVATE KEY").into_iter().next().ok_or("no private key in key.pem")?;
    let config = ServerConfig::builder_with_provider(Arc::new(tokio_rustls::rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_no_client_auth()
        .with_single_cert(certs, PrivateKeyDer::try_from(key).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    Ok((TlsAcceptor::from(Arc::new(config)), cert_path))
}

/// The DER bytes of every PEM block with this label.
fn pem_blocks(pem: &[u8], label: &str) -> Vec<Vec<u8>> {
    let text = String::from_utf8_lossy(pem);
    let (begin, end) = (format!("-----BEGIN {label}-----"), format!("-----END {label}-----"));
    let mut out = vec![];
    let mut rest = &text[..];
    while let Some(i) = rest.find(&begin) {
        let after = &rest[i + begin.len()..];
        let Some(j) = after.find(&end) else { break };
        let b64: String = after[..j].chars().filter(|c| !c.is_whitespace()).collect();
        if let Some(der) = base64(&b64) {
            out.push(der);
        }
        rest = &after[j + end.len()..];
    }
    out
}

fn base64(s: &str) -> Option<Vec<u8>> {
    let mut out = vec![];
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_certificate_is_made_and_loaded_again() {
        let dir = tempfile::tempdir().unwrap();
        let (_, path) = acceptor(dir.path(), vec!["192.168.1.2".into()]).unwrap();
        assert!(path.exists());
        acceptor(dir.path(), vec![]).unwrap();
    }

    #[test]
    fn base64_decodes() {
        assert_eq!(base64("aGVsbG8=").unwrap(), b"hello");
    }
}
