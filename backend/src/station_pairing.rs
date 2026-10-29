//! Explicit installation trust for native GroundStation/RRLS handover.
//! The private seed stays on the paired servers, never in a discovery response.
use crate::auth::{CalibrationAccess, CommandAccess, Permissions, StreamRole};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ring::signature::{self, KeyPair};
use serde::{Deserialize, Serialize};
use std::path::Path;

pub const PORT: u16 = 49363;
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Peer {
    pub id: String,
    pub role: String,
    #[serde(default)]
    pub urls: Vec<String>,
}
#[derive(Clone, Serialize, Deserialize, Debug)]
pub struct Public {
    pub installation_id: String,
    pub station_id: String,
    pub verify_key: String,
    pub peers: Vec<Peer>,
}
#[derive(Deserialize)]
struct FileConfig {
    version: u32,
    installation_id: String,
    station_id: String,
    signing_seed_file: String,
    peers: Vec<Peer>,
    #[serde(default = "default_port")]
    port: u16,
    #[serde(default = "default_scheme")]
    scheme: String,
}
fn default_port() -> u16 {
    443
}
fn default_scheme() -> String {
    "https".into()
}
pub struct Pairing {
    pub public: Public,
    key: signature::Ed25519KeyPair,
    port: u16,
    scheme: String,
}
#[derive(Serialize, Deserialize, Clone)]
pub struct Claims {
    pub installation_id: String,
    pub issuer: String,
    pub username: String,
    pub issued_at_ms: i64,
    pub expires_at_ms: i64,
    pub nonce: String,
    pub roles: Vec<StreamRole>,
    pub permissions: Permissions,
    pub command_access: CommandAccess,
    pub calibration_access: CalibrationAccess,
    pub remembered: bool,
}
#[derive(Serialize)]
pub struct Envelope {
    pub payload: String,
    pub signature: String,
}
#[derive(Serialize)]
struct Discovery<'a> {
    nonce: &'a str,
    installation_id: &'a str,
    station_id: &'a str,
    role: &'a str,
    port: u16,
    scheme: &'a str,
}
impl Pairing {
    pub fn load() -> anyhow::Result<Option<Self>> {
        let Some(path) = std::env::var_os("GS_STATION_PAIRING_FILE") else {
            return Ok(None);
        };
        Self::from_file(Path::new(&path)).map(Some)
    }
    pub(crate) fn from_file(path: &Path) -> anyhow::Result<Self> {
        let c: FileConfig = serde_json::from_slice(&std::fs::read(path)?)?;
        anyhow::ensure!(
            c.version == 1 && !c.installation_id.is_empty() && !c.station_id.is_empty(),
            "invalid station pairing identity"
        );
        anyhow::ensure!(
            c.port != 0 && c.peers.len() <= 8 && matches!(c.scheme.as_str(), "http" | "https"),
            "invalid station pairing limits"
        );
        let mut ids = std::collections::HashSet::new();
        for peer in &c.peers {
            anyhow::ensure!(
                peer.id != c.station_id
                    && ids.insert(&peer.id)
                    && matches!(peer.role.as_str(), "primary" | "rrls"),
                "invalid paired peer"
            );
            anyhow::ensure!(
                peer.urls.len() <= 8
                    && peer
                        .urls
                        .iter()
                        .all(|u| (u.starts_with("https://") || u.starts_with("http://"))
                            && !u.contains(['@', '?', '#'])),
                "paired URLs must be HTTP(S) origins"
            );
        }
        let seed_path = path
            .parent()
            .unwrap_or(Path::new("."))
            .join(&c.signing_seed_file);
        let seed = std::fs::read(seed_path)?;
        anyhow::ensure!(
            seed.len() == 32,
            "station signing seed must contain 32 bytes"
        );
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&seed)
            .map_err(|_| anyhow::anyhow!("invalid station signing seed"))?;
        Ok(Self {
            public: Public {
                installation_id: c.installation_id,
                station_id: c.station_id,
                verify_key: B64.encode(key.public_key().as_ref()),
                peers: c.peers,
            },
            key,
            port: c.port,
            scheme: c.scheme,
        })
    }
    pub(crate) fn mission_signing_key(&self) -> &signature::Ed25519KeyPair {
        &self.key
    }
    fn sign(&self, domain: &str, payload: &str) -> String {
        B64.encode(
            self.key
                .sign(format!("{domain}.{payload}").as_bytes())
                .as_ref(),
        )
    }
    pub fn token(&self, claims: &Claims) -> anyhow::Result<String> {
        let payload = B64.encode(serde_json::to_vec(claims)?);
        Ok(format!(
            "paired1.{payload}.{}",
            self.sign("paired1", &payload)
        ))
    }
    pub fn verify(&self, token: &str, now: i64) -> anyhow::Result<Claims> {
        anyhow::ensure!(token.len() <= 8192, "oversized paired session");
        let parts: Vec<_> = token.split('.').collect();
        anyhow::ensure!(
            parts.len() == 3 && parts[0] == "paired1",
            "invalid paired session"
        );
        signature::UnparsedPublicKey::new(&signature::ED25519, self.key.public_key().as_ref())
            .verify(
                format!("paired1.{}", parts[1]).as_bytes(),
                &B64.decode(parts[2])?,
            )
            .map_err(|_| anyhow::anyhow!("invalid paired session signature"))?;
        let claims: Claims = serde_json::from_slice(&B64.decode(parts[1])?)?;
        anyhow::ensure!(
            claims.installation_id == self.public.installation_id
                && (claims.issuer == self.public.station_id
                    || self.public.peers.iter().any(|p| p.id == claims.issuer)),
            "unpaired session issuer"
        );
        anyhow::ensure!(
            claims.issued_at_ms <= now.saturating_add(30_000)
                && claims.expires_at_ms > now
                && claims.expires_at_ms > claims.issued_at_ms,
            "paired session expired or clock invalid"
        );
        Ok(claims)
    }
    pub fn discovery(&self, nonce: &str) -> anyhow::Result<Envelope> {
        anyhow::ensure!(
            (16..=96).contains(&nonce.len())
                && nonce
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "invalid discovery nonce"
        );
        let d = Discovery {
            nonce,
            installation_id: &self.public.installation_id,
            station_id: &self.public.station_id,
            role: if crate::rrls::is_rrls() {
                "rrls"
            } else {
                "primary"
            },
            port: self.port,
            scheme: &self.scheme,
        };
        let payload = B64.encode(serde_json::to_vec(&d)?);
        Ok(Envelope {
            signature: self.sign("station1", &payload),
            payload,
        })
    }
}
pub fn public() -> Option<Public> {
    Pairing::load().ok().flatten().map(|p| p.public)
}
#[derive(Deserialize)]
pub struct DiscoveryQuery {
    nonce: String,
}
fn add_discovery_cors(
    pairing: &Pairing,
    headers: &axum::http::HeaderMap,
    response: &mut axum::response::Response,
) -> bool {
    let Some(origin) = headers
        .get(axum::http::header::ORIGIN)
        .and_then(|v| v.to_str().ok())
    else {
        return true;
    };
    let allowed = pairing
        .public
        .peers
        .iter()
        .flat_map(|p| &p.urls)
        .any(|u| u.trim_end_matches('/') == origin);
    if allowed {
        let out = response.headers_mut();
        out.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN,
            headers[axum::http::header::ORIGIN].clone(),
        );
        out.insert(
            axum::http::header::ACCESS_CONTROL_ALLOW_METHODS,
            axum::http::HeaderValue::from_static("GET, OPTIONS"),
        );
        out.insert(
            "access-control-allow-private-network",
            axum::http::HeaderValue::from_static("true"),
        );
        out.insert(
            axum::http::header::VARY,
            axum::http::HeaderValue::from_static("Origin"),
        );
    }
    allowed
}
pub async fn discover(
    headers: axum::http::HeaderMap,
    axum::extract::Query(q): axum::extract::Query<DiscoveryQuery>,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    match Pairing::load().and_then(|p| p.ok_or_else(|| anyhow::anyhow!("station is not paired"))) {
        Ok(pairing) => match pairing.discovery(&q.nonce) {
            Ok(e) => {
                let mut response = axum::Json(e).into_response();
                add_discovery_cors(&pairing, &headers, &mut response);
                response
            }
            Err(_) => (
                axum::http::StatusCode::BAD_REQUEST,
                "invalid discovery request",
            )
                .into_response(),
        },
        Err(_) => (
            axum::http::StatusCode::NOT_FOUND,
            "paired discovery unavailable",
        )
            .into_response(),
    }
}
pub async fn discovery_preflight(headers: axum::http::HeaderMap) -> axum::response::Response {
    use axum::response::IntoResponse;
    let mut response = axum::http::StatusCode::NO_CONTENT.into_response();
    if let Ok(Some(pairing)) = Pairing::load() {
        if add_discovery_cors(&pairing, &headers, &mut response) {
            return response;
        }
    }
    axum::http::StatusCode::FORBIDDEN.into_response()
}
/// Public address hints never establish trust or authorize credential handover.
fn public_discovery(nonce: &str, scheme: &str, port: u16) -> Option<serde_json::Value> {
    if !(16..=64).contains(&nonce.len())
        || !nonce.bytes().all(|b| b.is_ascii_alphanumeric())
        || port == 0
    {
        return None;
    }
    let role = if crate::rrls::is_rrls() {
        "rrls"
    } else {
        "primary"
    };
    Some(serde_json::json!({"protocol":"GS26-FIND1", "nonce":nonce,
        "name":if role == "rrls" { "RRLS" } else { "GroundStation" },
        "role":role, "scheme":scheme, "port":port}))
}
/// One listener handles public address selection and separately signed paired handover.
pub async fn serve_discovery(
    pairing: Option<Pairing>,
    scheme: &'static str,
    address: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let addr =
        std::env::var("GS_STATION_DISCOVERY_BIND").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));
    let socket = tokio::net::UdpSocket::bind(addr).await?;
    serve_discovery_socket(socket, pairing, scheme, address).await
}
async fn serve_discovery_socket(
    socket: tokio::net::UdpSocket,
    pairing: Option<Pairing>,
    scheme: &'static str,
    address: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let mut bytes = [0; 128];
    let mut last = std::time::Instant::now() - std::time::Duration::from_secs(1);
    loop {
        let (len, from) = socket.recv_from(&mut bytes).await?;
        if last.elapsed() < std::time::Duration::from_millis(50) {
            continue;
        }
        let Ok(request) = std::str::from_utf8(&bytes[..len]) else {
            continue;
        };
        let response = if let Some(nonce) = request.strip_prefix("GS26-FIND1 ") {
            if address.ip().is_loopback() && !from.ip().is_loopback() {
                continue;
            }
            public_discovery(nonce, scheme, address.port())
        } else if let (Some(nonce), Some(pairing)) =
            (request.strip_prefix("GS26-STATION1 "), pairing.as_ref())
        {
            pairing
                .discovery(nonce)
                .ok()
                .and_then(|e| serde_json::to_value(e).ok())
        } else {
            None
        };
        if let Some(response) = response {
            last = std::time::Instant::now();
            socket
                .send_to(&serde_json::to_vec(&response)?, from)
                .await?;
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(id: &str, seed: u8) -> Pairing {
        let key = signature::Ed25519KeyPair::from_seed_unchecked(&[seed; 32]).unwrap();
        Pairing {
            public: Public {
                installation_id: "test".into(),
                station_id: id.into(),
                verify_key: B64.encode(key.public_key().as_ref()),
                peers: vec![Peer {
                    id: if id == "primary" { "rrls" } else { "primary" }.into(),
                    role: "rrls".into(),
                    urls: vec![],
                }],
            },
            key,
            port: 443,
            scheme: "https".into(),
        }
    }
    #[tokio::test]
    async fn unpaired_public_discovery_replies_but_does_not_create_handover_trust() {
        let server = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let endpoint = server.local_addr().unwrap();
        let task = tokio::spawn(serve_discovery_socket(
            server,
            None,
            "https",
            "127.0.0.1:8443".parse().unwrap(),
        ));
        let client = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
        client
            .send_to(b"GS26-FIND1 0123456789abcdef", endpoint)
            .await
            .unwrap();
        let mut buffer = [0; 1024];
        let (length, from) = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            client.recv_from(&mut buffer),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(from, endpoint);
        let response: serde_json::Value = serde_json::from_slice(&buffer[..length]).unwrap();
        assert_eq!(response["protocol"], "GS26-FIND1");
        assert_eq!(response["port"], 8443);
        assert_eq!(response["scheme"], "https");
        assert_eq!(response["nonce"], "0123456789abcdef");
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        client
            .send_to(b"GS26-STATION1 0123456789abcdef", endpoint)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(80),
                client.recv_from(&mut buffer)
            )
            .await
            .is_err()
        );
        task.abort();
        let _ = task.await;
    }
    #[test]
    fn public_discovery_is_nonce_bound_address_only() {
        assert!(public_discovery("short", "https", 443).is_none());
        assert!(public_discovery("0123456789abcdef?", "https", 443).is_none());
        assert!(public_discovery(&"a".repeat(65), "https", 443).is_none());
        assert!(public_discovery("0123456789abcdef", "https", 0).is_none());
        let d = public_discovery("0123456789abcdef", "https", 8443).unwrap();
        assert_eq!(d["port"], 8443);
        assert_eq!(d["nonce"], "0123456789abcdef");
        assert!(d.get("token").is_none());
        assert!(d.get("verify_key").is_none());
    }
    #[test]
    fn paired_sessions_reject_wrong_keys_groups_issuers_tampering_and_expiry() {
        let a = fixture("primary", 1);
        let b = fixture("rrls", 1);
        let mut claims = Claims {
            installation_id: "test".into(),
            issuer: "primary".into(),
            username: "operator".into(),
            issued_at_ms: 10,
            expires_at_ms: 100,
            nonce: "unique".into(),
            roles: vec![],
            permissions: Permissions::default(),
            command_access: CommandAccess::default(),
            calibration_access: CalibrationAccess::default(),
            remembered: true,
        };
        let token = a.token(&claims).unwrap();
        assert_eq!(b.verify(&token, 50).unwrap().username, "operator");
        assert!(fixture("rrls", 2).verify(&token, 50).is_err());
        assert!(b.verify(&token, 100).is_err());
        assert!(b.verify(&(token.clone() + "x"), 50).is_err());
        claims.installation_id = "other".into();
        assert!(b.verify(&a.token(&claims).unwrap(), 50).is_err());
        claims.installation_id = "test".into();
        claims.issuer = "unpaired".into();
        assert!(b.verify(&a.token(&claims).unwrap(), 50).is_err());
    }
    #[test]
    fn discovery_is_nonce_bound_and_domain_separated_without_secrets() {
        let p = fixture("primary", 1);
        let e = p.discovery("0123456789abcdef").unwrap();
        let raw = serde_json::to_value(&e).unwrap();
        assert_eq!(raw.as_object().unwrap().len(), 2);
        let key =
            signature::UnparsedPublicKey::new(&signature::ED25519, p.key.public_key().as_ref());
        let signature = B64.decode(&e.signature).unwrap();
        assert!(
            key.verify(format!("station1.{}", e.payload).as_bytes(), &signature)
                .is_ok()
        );
        assert!(
            key.verify(format!("paired1.{}", e.payload).as_bytes(), &signature)
                .is_err()
        );
        assert!(p.discovery("tiny").is_err());
        assert!(p.discovery(&"x".repeat(97)).is_err());
    }
    #[test]
    fn discovery_cors_is_limited_to_configured_paired_origins() {
        use axum::response::IntoResponse;
        let mut p = fixture("primary", 1);
        p.public.peers[0].urls = vec!["http://rrls.local:3000".into()];
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            axum::http::header::ORIGIN,
            "http://rrls.local:3000".parse().unwrap(),
        );
        let mut response = axum::http::StatusCode::OK.into_response();
        assert!(add_discovery_cors(&p, &headers, &mut response));
        assert_eq!(
            response.headers()[axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN],
            "http://rrls.local:3000"
        );
        headers.insert(
            axum::http::header::ORIGIN,
            "http://unpaired.example".parse().unwrap(),
        );
        let mut response = axum::http::StatusCode::OK.into_response();
        assert!(!add_discovery_cors(&p, &headers, &mut response));
        assert!(
            !response
                .headers()
                .contains_key(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
        );
    }
}
