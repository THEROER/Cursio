//! A certificate authority made on the spot, and a server certificate it
//! signed, as the configurations rustls wants: what the TLS tests and the
//! panel example run on instead of a real CA.

use std::sync::Arc;

use cursio_channel::tls::rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use cursio_channel::tls::rustls::{self, RootCertStore};
use cursio_channel::tls::{ClientConfig, ServerConfig};
use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

pub struct Certs {
    pub server: Arc<ServerConfig>,
    pub client: Arc<ClientConfig>,
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// A CA, and a certificate for `server_name` signed by it.
pub fn certs(server_name: &str) -> Certs {
    let ca_key = KeyPair::generate().expect("a CA key");
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA parameters");
    ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca.self_signed(&ca_key).expect("a self-signed CA");

    let key = KeyPair::generate().expect("a server key");
    let leaf = CertificateParams::new(vec![server_name.to_string()])
        .expect("server parameters")
        .signed_by(&key, &ca, &ca_key)
        .expect("a server certificate");

    let mut roots = RootCertStore::empty();
    roots.add(ca.der().clone()).expect("the CA as a root");
    let client = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    let server = ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(leaf.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("a server configuration");
    Certs {
        server: Arc::new(server),
        client: Arc::new(client),
    }
}
