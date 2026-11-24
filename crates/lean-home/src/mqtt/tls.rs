//! TLS for the broker connection, from the settings HA's MQTT config entry stores: `certificate`
//! (a CA as PEM text, or "auto" for the public roots), `client_cert`/`client_key` (PEM text),
//! `tls_insecure` (skip the hostname check only, as paho's `tls_insecure_set`).

use std::sync::Arc;

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{
    CertificateError, ClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
};

pub struct Tls {
    pub certificate: String,
    pub client_cert: Option<String>,
    pub client_key: Option<String>,
    pub insecure: bool,
}

impl Tls {
    pub fn client_config(&self) -> Result<ClientConfig, String> {
        let mut roots = RootCertStore::empty();
        if self.certificate == "auto" {
            // HA uses certifi, i.e. Mozilla's bundle; webpki-roots is the same list.
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        } else {
            for cert in CertificateDer::pem_slice_iter(self.certificate.as_bytes()) {
                roots
                    .add(cert.map_err(|e| format!("mqtt CA certificate: {e}"))?)
                    .map_err(|e| format!("mqtt CA certificate: {e}"))?;
            }
            if roots.is_empty() {
                return Err("mqtt CA certificate: no certificate found in PEM".into());
            }
        }
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let builder = ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?;
        let roots = Arc::new(roots);
        let builder = if self.insecure {
            let inner = WebPkiServerVerifier::builder_with_provider(roots, provider)
                .build()
                .map_err(|e| e.to_string())?;
            builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AnyHostname(inner)))
        } else {
            builder.with_root_certificates(roots)
        };
        match (&self.client_cert, &self.client_key) {
            (Some(cert), Some(key)) => {
                let chain = CertificateDer::pem_slice_iter(cert.as_bytes())
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|e| format!("mqtt client certificate: {e}"))?;
                let key = PrivateKeyDer::from_pem_slice(key.as_bytes())
                    .map_err(|e| format!("mqtt client key: {e}"))?;
                builder
                    .with_client_auth_cert(chain, key)
                    .map_err(|e| format!("mqtt client certificate: {e}"))
            }
            _ => Ok(builder.with_no_client_auth()),
        }
    }
}

/// Full chain verification, but any hostname: what `tls_insecure` means in HA.
#[derive(Debug)]
struct AnyHostname(Arc<WebPkiServerVerifier>);

impl ServerCertVerifier for AnyHostname {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        match self
            .0
            .verify_server_cert(end_entity, intermediates, server_name, ocsp, now)
        {
            Err(rustls::Error::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            other => other,
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.0.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.supported_verify_schemes()
    }
}
