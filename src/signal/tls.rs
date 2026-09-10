//! Certificate-authenticated signaling aliases. A room's HTTPS-authenticated
//! endpoint roster can include a bare address of its DNS-named service.
//! Keep IP routing / no DNS SNI, but authenticate that service's configured
//! DNS identity instead of accepting an arbitrary certificate at the address.

use std::sync::Arc;

use anyhow::{Context, Result, bail};
use rustls::{
    CertificateError, DigitallySignedStruct, Error, SignatureScheme,
    client::{
        WebPkiServerVerifier,
        danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    },
    pki_types::{CertificateDer, ServerName, UnixTime},
};

#[derive(Debug)]
struct RoomServiceVerifier {
    verifier: Arc<WebPkiServerVerifier>,
    identities: Vec<ServerName<'static>>,
}

impl ServerCertVerifier for RoomServiceVerifier {
    fn verify_server_cert(
        &self,
        leaf: &CertificateDer<'_>,
        chain: &[CertificateDer<'_>],
        name: &ServerName<'_>,
        ocsp: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        let error = match self
            .verifier
            .verify_server_cert(leaf, chain, name, ocsp, now)
        {
            Ok(verified) => return Ok(verified),
            Err(error) => error,
        };
        if matches!(name, ServerName::IpAddress(_))
            && matches!(
                error,
                Error::InvalidCertificate(
                    CertificateError::NotValidForName
                        | CertificateError::NotValidForNameContext { .. }
                )
            )
        {
            for identity in &self.identities {
                if let Ok(verified) = self
                    .verifier
                    .verify_server_cert(leaf, chain, identity, ocsp, now)
                {
                    tracing::debug!(address = ?name, service_identity = ?identity, "authenticated signaling IP alias using the room's DNS service identity");
                    return Ok(verified);
                }
            }
        }
        Err(error)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.verifier.verify_tls12_signature(message, cert, signed)
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signed: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.verifier.verify_tls13_signature(message, cert, signed)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.verifier.supported_verify_schemes()
    }
}

pub(super) fn client_config(endpoints: &[String]) -> Result<Arc<rustls::ClientConfig>> {
    let certificates = rustls_native_certs::load_native_certs();
    if certificates.certs.is_empty() {
        bail!("no native TLS root certificates: {:?}", certificates.errors);
    }
    let mut roots = rustls::RootCertStore::empty();
    roots.add_parsable_certificates(certificates.certs);
    let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
        .build()
        .context("create signaling certificate verifier")?;
    let mut identities = Vec::new();
    for endpoint in endpoints {
        let url = if endpoint.contains("://") {
            endpoint.clone()
        } else {
            format!("wss://{endpoint}")
        };
        let url = url::Url::parse(&url).context("invalid signaling endpoint")?;
        if url.scheme() != "wss" {
            bail!("signaling endpoint must use WSS");
        }
        let host = url
            .host_str()
            .context("signaling endpoint has no host")?
            .trim_matches(['[', ']'])
            .to_owned();
        let name = ServerName::try_from(host).context("invalid signaling certificate identity")?;
        if matches!(name, ServerName::DnsName(_)) && !identities.contains(&name) {
            identities.push(name);
        }
    }
    Ok(Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(RoomServiceVerifier {
                verifier,
                identities,
            }))
            .with_no_client_auth(),
    ))
}
