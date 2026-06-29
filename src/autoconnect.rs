//! Outbound "auto-connect": the adapter dials a configured server and bridges
//! SLCAN over the connection (so it works from behind NAT), optionally over TLS
//! with server public-key pinning.
//!
//! Target is a single URL:
//!   `tcp://host:port/`
//!   `tls://host:port/?pubkey=<hex>&token=<hex>`
//!
//! * `token` — if present, `H<token>\r` is sent right after the link is up
//!   (identifies this adapter to the server).
//! * `pubkey` — if present (uncompressed P-256, `04`+64 bytes = 130 hex), the TLS
//!   server is authenticated by pinning this key: the handshake's CertificateVerify
//!   signature must validate under it, else the connection is dropped. Requires
//!   `tls://`. Without `pubkey`, TLS is encrypt-only (no server authentication).
//!
//! Reconnects with a backoff; current status is published in `AUTOCONNECT_STATE`.

use core::net::Ipv4Addr;
use core::sync::atomic::Ordering;

use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, Stack};
use embassy_time::{Duration, Timer};
use embedded_tls::{
    Aes128GcmSha256, Certificate, CertificateEntryRef, CertificateRef, CertificateVerifyRef,
    CryptoProvider, FlushPolicy, SignatureScheme, TlsConfig, TlsConnection, TlsContext, TlsError,
    TlsVerifier,
};
use rand_core::{CryptoRng, CryptoRngCore, RngCore};
use sha2::{Digest, Sha256};

use crate::{
    can, transport, AUTOCONNECT_CONFIG, AUTOCONNECT_STATE, SERVER_PUBKEY, TCP_CLIENTS,
    WIFI_CONNECTED,
};

const TLS_BUF: usize = 16_640;

/// Auto-connect status codes (published in `AUTOCONNECT_STATE`).
pub mod state {
    pub const DISABLED: u8 = 0;
    pub const CONNECTING: u8 = 1;
    pub const CONNECTED: u8 = 2;
    pub const DNS_FAIL: u8 = 3;
    pub const TCP_FAIL: u8 = 4;
    pub const TLS_FAIL: u8 = 5;
    pub const BAD_CONFIG: u8 = 6;
    pub const PIN_MISMATCH: u8 = 7;
}

/// Reconnect backoff (seconds) after a key-pinning mismatch — long, since it
/// won't fix itself until the config or server changes.
const PIN_MISMATCH_BACKOFF_S: u64 = 90;

pub fn state_str(code: u8) -> &'static str {
    match code {
        state::DISABLED => "disabled",
        state::CONNECTING => "connecting",
        state::CONNECTED => "connected",
        state::DNS_FAIL => "DNS lookup failed",
        state::TCP_FAIL => "TCP connect failed",
        state::TLS_FAIL => "TLS failed (cert/handshake)",
        state::BAD_CONFIG => "bad URL or pubkey",
        state::PIN_MISMATCH => "server pubkey mismatch",
        _ => "?",
    }
}

fn set_state(s: u8) {
    AUTOCONNECT_STATE.store(s, Ordering::Relaxed);
}

// --- Hardware RNG for embedded-tls ------------------------------------------
struct EspRng;
impl RngCore for EspRng {
    fn next_u32(&mut self) -> u32 {
        esp_hal::rng::Rng::new().random()
    }
    fn next_u64(&mut self) -> u64 {
        ((self.next_u32() as u64) << 32) | self.next_u32() as u64
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let r = self.next_u32().to_le_bytes();
            chunk.copy_from_slice(&r[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
impl CryptoRng for EspRng {}

/// Extract an uncompressed P-256 public key (`04`+X+Y, 65 bytes) from a DER cert
/// by finding its SubjectPublicKeyInfo BIT STRING (`03 42 00 04` + 64 bytes).
fn extract_p256_pubkey(der: &[u8]) -> Option<[u8; 65]> {
    let i = der.windows(4).position(|w| w == [0x03, 0x42, 0x00, 0x04])?;
    let start = i + 3; // index of the 0x04 point prefix
    let slice = der.get(start..start + 65)?;
    let mut key = [0u8; 65];
    key.copy_from_slice(slice);
    Some(key)
}

// --- TLS verifier: capture the server key, and optionally pin it ------------
//
// Always extracts the server's P-256 key and publishes it (for the web page).
// `verify_signature` authenticates the server by checking the handshake
// CertificateVerify signature under that key; if a `pin` is configured, the key
// must also equal it. With no key extractable and no pin, it falls back to
// encrypt-only (no authentication).

struct PinnedVerifier {
    pin: Option<[u8; 65]>,
    server_key: Option<[u8; 65]>,
    transcript: Option<Sha256>,
}

impl TlsVerifier<Aes128GcmSha256> for PinnedVerifier {
    fn set_hostname_verification(&mut self, _hostname: &str) -> Result<(), TlsError> {
        Ok(()) // we pin the key, not the hostname
    }

    fn verify_certificate(
        &mut self,
        transcript: &Sha256,
        _ca: &Option<Certificate>,
        cert: CertificateRef,
    ) -> Result<(), TlsError> {
        self.server_key = match cert.entries.first() {
            Some(CertificateEntryRef::X509(der)) => extract_p256_pubkey(der),
            _ => None,
        };
        // Publish for the web UI (so the user can copy it into `pubkey=`).
        SERVER_PUBKEY.lock(|c| c.set(self.server_key));
        self.transcript = Some(transcript.clone());
        Ok(())
    }

    fn verify_signature(&mut self, verify: CertificateVerifyRef) -> Result<(), TlsError> {
        let Some(key) = self.server_key else {
            // Could not read the key. Only acceptable when not pinning (encrypt-only).
            return if self.pin.is_some() {
                Err(TlsError::InvalidCertificate)
            } else {
                Ok(())
            };
        };
        if let Some(pin) = self.pin {
            if pin != key {
                set_state(state::PIN_MISMATCH); // flag for the long reconnect backoff
                return Err(TlsError::InvalidCertificate);
            }
        }

        let hash = self.transcript.take().ok_or(TlsError::DecodeError)?;
        // TLS 1.3 server CertificateVerify signed data.
        let mut msg = [0u8; 130];
        for b in &mut msg[..64] {
            *b = 0x20;
        }
        msg[64..98].copy_from_slice(b"TLS 1.3, server CertificateVerify\x00");
        msg[98..130].copy_from_slice(&hash.finalize());

        match verify.signature_scheme {
            SignatureScheme::EcdsaSecp256r1Sha256 => {
                use p256::ecdsa::{signature::Verifier, Signature, VerifyingKey};
                let vk =
                    VerifyingKey::from_sec1_bytes(&key).map_err(|_| TlsError::DecodeError)?;
                let sig =
                    Signature::from_der(verify.signature).map_err(|_| TlsError::DecodeError)?;
                vk.verify(&msg, &sig).map_err(|_| TlsError::InvalidSignature)
            }
            _ => Err(TlsError::InvalidSignatureScheme),
        }
    }
}

struct PinnedProvider {
    rng: EspRng,
    verifier: PinnedVerifier,
}

impl PinnedProvider {
    fn new(pin: Option<[u8; 65]>) -> Self {
        Self {
            rng: EspRng,
            verifier: PinnedVerifier {
                pin,
                server_key: None,
                transcript: None,
            },
        }
    }
}

impl CryptoProvider for PinnedProvider {
    type CipherSuite = Aes128GcmSha256;
    type Signature = p256::ecdsa::DerSignature;

    fn rng(&mut self) -> impl CryptoRngCore {
        &mut self.rng
    }

    fn verifier(&mut self) -> Result<&mut impl TlsVerifier<Self::CipherSuite>, TlsError> {
        Ok(&mut self.verifier)
    }
}

// --- URL parsing ------------------------------------------------------------
struct ParsedUrl<'a> {
    tls: bool,
    host: &'a str,
    port: u16,
    pubkey: Option<&'a str>,
    token: Option<&'a str>,
}

fn parse_url(url: &str) -> Option<ParsedUrl<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    let tls = match scheme {
        "tls" => true,
        "tcp" => false,
        _ => return None,
    };
    let (before_q, query) = match rest.split_once('?') {
        Some((b, q)) => (b, Some(q)),
        None => (rest, None),
    };
    let authority = before_q.trim_end_matches('/');
    let (host, port_str) = authority.rsplit_once(':')?;
    let port: u16 = port_str.parse().ok()?;
    if host.is_empty() {
        return None;
    }

    let mut pubkey = None;
    let mut token = None;
    if let Some(q) = query {
        for kv in q.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                match k {
                    "pubkey" => pubkey = Some(v),
                    "token" => token = Some(v),
                    _ => {}
                }
            }
        }
    }
    Some(ParsedUrl {
        tls,
        host,
        port,
        pubkey,
        token,
    })
}

fn hexval(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Decode a 130-hex-char uncompressed P-256 point (`04`+X+Y) into 65 bytes.
fn parse_pubkey(hex: &str) -> Option<[u8; 65]> {
    let b = hex.as_bytes();
    if b.len() != 130 {
        return None;
    }
    let mut out = [0u8; 65];
    for i in 0..65 {
        out[i] = (hexval(b[2 * i])? << 4) | hexval(b[2 * i + 1])?;
    }
    if out[0] != 0x04 {
        return None;
    }
    Some(out)
}

async fn resolve(stack: Stack<'static>, host: &str) -> Option<Ipv4Addr> {
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Some(ip);
    }
    let answers = stack.dns_query(host, DnsQueryType::A).await.ok()?;
    answers.iter().find_map(|a| match a {
        IpAddress::Ipv4(v4) => Some(*v4),
        #[allow(unreachable_patterns)]
        _ => None,
    })
}

/// Send the `H<token>\r` auth command, if a token is configured.
async fn send_token<C: embedded_io_async::Write>(conn: &mut C, token: Option<&str>) {
    if let Some(tok) = token {
        let _ = conn.write_all(b"H").await;
        let _ = conn.write_all(tok.as_bytes()).await;
        let _ = conn.write_all(b"\r").await;
        let _ = conn.flush().await; // TLS buffers writes until flushed
    }
}

#[embassy_executor::task]
pub async fn autoconnect_task(stack: Stack<'static>) {
    loop {
        let cfg = *AUTOCONNECT_CONFIG.lock().await;
        if !cfg.enabled() {
            set_state(state::DISABLED);
            Timer::after(Duration::from_secs(3)).await;
            continue;
        }
        if !WIFI_CONNECTED.load(Ordering::Relaxed) {
            Timer::after(Duration::from_secs(3)).await;
            continue;
        }

        let Some(url) = parse_url(cfg.url_str()) else {
            set_state(state::BAD_CONFIG);
            Timer::after(Duration::from_secs(30)).await;
            continue;
        };
        // pubkey, if present, must be valid and used over TLS.
        let pin = match url.pubkey {
            Some(h) => match parse_pubkey(h) {
                Some(k) if url.tls => Some(k),
                _ => {
                    set_state(state::BAD_CONFIG);
                    Timer::after(Duration::from_secs(30)).await;
                    continue;
                }
            },
            None => None,
        };

        set_state(state::CONNECTING);
        let Some(ip) = resolve(stack, url.host).await else {
            set_state(state::DNS_FAIL);
            Timer::after(Duration::from_secs(5)).await;
            continue;
        };

        let mut rxb = [0u8; 1024];
        let mut txb = [0u8; 1024];
        let mut socket = TcpSocket::new(stack, &mut rxb, &mut txb);
        socket.set_timeout(Some(Duration::from_secs(300)));
        if socket.connect((ip, url.port)).await.is_err() {
            set_state(state::TCP_FAIL);
            Timer::after(Duration::from_secs(5)).await;
            continue;
        }

        let mut sub = can::rx_subscriber();
        TCP_CLIENTS.fetch_add(1, Ordering::Relaxed);

        if url.tls {
            let mut rb = alloc::vec![0u8; TLS_BUF];
            let mut wb = alloc::vec![0u8; TLS_BUF];
            let tls_config = TlsConfig::new().with_server_name(url.host);
            let mut tls: TlsConnection<TcpSocket, Aes128GcmSha256> =
                TlsConnection::new(socket, &mut rb[..], &mut wb[..]);
            // Emit records without blocking on a per-flush TCP ACK.
            tls.set_flush_policy(FlushPolicy::Relaxed);
            // Always use the capturing verifier so the server key is published
            // (and pinned when `pin` is set).
            let opened = tls
                .open(TlsContext::new(&tls_config, PinnedProvider::new(pin)))
                .await
                .is_ok();
            if opened {
                set_state(state::CONNECTED);
                send_token(&mut tls, url.token).await;
                transport::run_port(&mut tls, &mut sub, transport::Iface::Tcp).await;
            } else if AUTOCONNECT_STATE.load(Ordering::Relaxed) != state::PIN_MISMATCH {
                // keep PIN_MISMATCH (set by the verifier); otherwise generic failure
                set_state(state::TLS_FAIL);
            }
            let _ = tls.close().await;
        } else {
            set_state(state::CONNECTED);
            send_token(&mut socket, url.token).await;
            transport::run_port(&mut socket, &mut sub, transport::Iface::Tcp).await;
            socket.close();
        }

        TCP_CLIENTS.fetch_sub(1, Ordering::Relaxed);
        // Back off hard on a pinned-key mismatch; otherwise reconnect promptly.
        let backoff = if AUTOCONNECT_STATE.load(Ordering::Relaxed) == state::PIN_MISMATCH {
            PIN_MISMATCH_BACKOFF_S
        } else {
            3
        };
        Timer::after(Duration::from_secs(backoff)).await;
    }
}
