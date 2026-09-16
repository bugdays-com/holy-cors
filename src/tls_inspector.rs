use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error, RootCertStore, SignatureScheme};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_rustls::TlsConnector;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MIN_TIMEOUT_MS: u64 = 1_000;
const MAX_TIMEOUT_MS: u64 = 15_000;
static VERIFIED_TLS_CONFIG: OnceLock<Result<Arc<ClientConfig>, String>> = OnceLock::new();

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsInspectRequest {
    pub host: String,
    pub port: u16,
    #[serde(default)]
    pub server_name: Option<String>,
    #[serde(default = "default_timeout")]
    pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
    DEFAULT_TIMEOUT_MS
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsInspectResponse {
    pub host: String,
    pub port: u16,
    pub server_name: String,
    pub peer_address: String,
    pub tls_version: Option<String>,
    pub cipher_suite: Option<String>,
    pub alpn: Option<String>,
    pub validation: TlsValidation,
    pub certificates: Vec<TlsCertificate>,
    pub connected_at: u128,
    pub elapsed_ms: u128,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsValidation {
    pub trusted: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TlsCertificate {
    pub position: usize,
    pub der_base64: String,
}

#[derive(Debug)]
struct ObservationalVerifier;

impl ServerCertVerifier for ObservationalVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &rustls::crypto::ring::default_provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

pub async fn inspect(request: TlsInspectRequest) -> Result<TlsInspectResponse, String> {
    let (host, server_name, timeout_ms) = validate_request(&request)?;
    timeout(
        Duration::from_millis(timeout_ms),
        inspect_with_timeout(host, request.port, server_name),
    )
    .await
    .map_err(|_| format!("Connection timed out after {} seconds.", timeout_ms / 1_000))?
}

async fn inspect_with_timeout(
    host: String,
    port: u16,
    server_name: String,
) -> Result<TlsInspectResponse, String> {
    let started = Instant::now();
    let (peer_address, connection) = connect_observational(&host, port, &server_name).await?;
    let (_, session) = connection.get_ref();
    let certificates = session
        .peer_certificates()
        .ok_or_else(|| "The service completed TLS without presenting a certificate.".to_string())?
        .iter()
        .enumerate()
        .map(|(position, certificate)| TlsCertificate {
            position,
            der_base64: STANDARD.encode(certificate.as_ref()),
        })
        .collect::<Vec<_>>();
    let tls_version = session.protocol_version().map(|value| format!("{value:?}"));
    let cipher_suite = session
        .negotiated_cipher_suite()
        .map(|value| format!("{:?}", value.suite()));
    let alpn = session
        .alpn_protocol()
        .map(|value| String::from_utf8_lossy(value).to_string());
    drop(connection);

    let validation_error = connect_verified(&host, port, &server_name).await.err();
    let trusted = validation_error.is_none();

    Ok(TlsInspectResponse {
        host,
        port,
        server_name,
        peer_address,
        tls_version,
        cipher_suite,
        alpn,
        validation: TlsValidation {
            trusted,
            error: validation_error,
        },
        certificates,
        connected_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
        elapsed_ms: started.elapsed().as_millis(),
    })
}

async fn connect_observational(
    host: &str,
    port: u16,
    server_name: &str,
) -> Result<(String, tokio_rustls::client::TlsStream<TcpStream>), String> {
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("Could not connect to {host}:{port}: {error}"))?;
    let peer_address = tcp
        .peer_addr()
        .map(|value| value.to_string())
        .unwrap_or_else(|_| format!("{host}:{port}"));
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(ObservationalVerifier))
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let name = owned_server_name(server_name)?;
    let connection = connector
        .connect(name, tcp)
        .await
        .map_err(|error| format!("TLS handshake failed for {host}:{port}: {error}"))?;
    Ok((peer_address, connection))
}

async fn connect_verified(host: &str, port: u16, server_name: &str) -> Result<(), String> {
    let connector = TlsConnector::from(verified_tls_config()?);
    let tcp = TcpStream::connect((host, port))
        .await
        .map_err(|error| format!("Trust check could not reconnect: {error}"))?;
    connector
        .connect(owned_server_name(server_name)?, tcp)
        .await
        .map_err(|error| friendly_trust_error(&error.to_string()))?;
    Ok(())
}

fn verified_tls_config() -> Result<Arc<ClientConfig>, String> {
    VERIFIED_TLS_CONFIG
        .get_or_init(|| {
            let native = rustls_native_certs::load_native_certs();
            let mut roots = RootCertStore::empty();
            let (added, _) = roots.add_parsable_certificates(native.certs);
            if added == 0 {
                return Err(
                    "No trusted root certificates were available on this device.".to_string(),
                );
            }
            Ok(Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(roots)
                    .with_no_client_auth(),
            ))
        })
        .clone()
}

fn validate_request(request: &TlsInspectRequest) -> Result<(String, String, u64), String> {
    let host = request.host.trim().trim_matches(['[', ']']).to_string();
    if host.is_empty() || host.len() > 253 || host.chars().any(char::is_whitespace) {
        return Err("Enter one valid hostname or IP address.".to_string());
    }
    if host.contains("://") || host.contains('/') || host.contains('@') {
        return Err(
            "Enter a hostname or IP address without a URL, path, or credentials.".to_string(),
        );
    }
    if request.port == 0 {
        return Err("Port must be between 1 and 65535.".to_string());
    }
    let server_name = request
        .server_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(&host)
        .trim_end_matches('.')
        .to_string();
    owned_server_name(&server_name)?;
    Ok((
        host,
        server_name,
        request.timeout_ms.clamp(MIN_TIMEOUT_MS, MAX_TIMEOUT_MS),
    ))
}

fn owned_server_name(value: &str) -> Result<ServerName<'static>, String> {
    ServerName::try_from(value.to_string())
        .map_err(|_| "The TLS server name must be a valid hostname or IP address.".to_string())
}

fn friendly_trust_error(error: &str) -> String {
    let lower = error.to_ascii_lowercase();
    if lower.contains("not valid for name") || lower.contains("notvalidforname") {
        "The certificate does not cover the requested server name.".to_string()
    } else if lower.contains("expired") {
        "The certificate chain contains an expired certificate.".to_string()
    } else if lower.contains("not valid yet") {
        "The certificate chain is not valid yet.".to_string()
    } else if lower.contains("unknownissuer") || lower.contains("unknown issuer") {
        "The certificate chain is not trusted by this device (unknown issuer or missing intermediate).".to_string()
    } else {
        format!("The certificate chain was presented but did not pass this device's trust check: {error}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ip_target_with_dns_sni() {
        let request = TlsInspectRequest {
            host: "10.0.0.8".to_string(),
            port: 9093,
            server_name: Some("broker.internal".to_string()),
            timeout_ms: 999_999,
        };
        let (_, server_name, timeout_ms) = validate_request(&request).unwrap();
        assert_eq!(server_name, "broker.internal");
        assert_eq!(timeout_ms, MAX_TIMEOUT_MS);
    }

    #[test]
    fn rejects_urls_and_zero_port() {
        let request = TlsInspectRequest {
            host: "https://example.com".to_string(),
            port: 0,
            server_name: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        };
        assert!(validate_request(&request).is_err());
    }
}
