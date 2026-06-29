//! Minimal mDNS responder so the device is reachable as `espcan-br.local` in STA
//! mode without scanning the network. Hand-rolled over an embassy-net UDP socket
//! (no extra crate); answers A-record queries for our hostname. Adapted from the
//! espwebserver reference. No logging (UART0 carries the SLCAN stream).

use core::net::Ipv4Addr;

use embassy_net::udp::{PacketMetadata, UdpSocket};
use embassy_net::{IpAddress, IpEndpoint, Ipv4Address, Stack};
use embassy_time::{Duration, Timer};
use heapless::Vec;

use crate::{DEVICE_CONFIG, STA_IP, WIFI_CONNECTED};

const MDNS_PORT: u16 = 5353;
/// Max hostname length (matches `DeviceConfig` name field).
const NAME_MAX: usize = 32;

fn mdns_endpoint(addr: Ipv4Addr) -> IpEndpoint {
    IpEndpoint::new(IpAddress::Ipv4(addr), MDNS_PORT)
}

#[embassy_executor::task]
pub async fn responder_task(stack: Stack<'static>) {
    // Wait for STA connectivity + an IP.
    let our_ip: Ipv4Addr = loop {
        if WIFI_CONNECTED.load(core::sync::atomic::Ordering::Relaxed) {
            if let Some(ip) = *STA_IP.lock().await {
                break ip;
            }
        }
        Timer::after(Duration::from_secs(1)).await;
    };
    let octets = our_ip.octets();

    // Snapshot the configured device name (changing it requires a reboot anyway).
    let mut name_buf = [0u8; NAME_MAX];
    let host: &[u8] = {
        let cfg = DEVICE_CONFIG.lock().await;
        let n = cfg.name_str();
        let len = n.len().min(NAME_MAX);
        name_buf[..len].copy_from_slice(&n.as_bytes()[..len]);
        &name_buf[..len]
    };

    if stack
        .join_multicast_group(Ipv4Address::new(224, 0, 0, 251))
        .is_err()
    {
        return;
    }

    let mut rx_meta = [PacketMetadata::EMPTY; 4];
    let mut tx_meta = [PacketMetadata::EMPTY; 4];
    let mut rx_buf = [0u8; 512];
    let mut tx_buf = [0u8; 512];
    let mut socket = UdpSocket::new(stack, &mut rx_meta, &mut rx_buf, &mut tx_meta, &mut tx_buf);
    if socket.bind(MDNS_PORT).is_err() {
        return;
    }

    // Unsolicited announcement to the multicast group.
    if let Some(ann) = build_announcement(&octets, host) {
        let _ = socket
            .send_to(&ann, mdns_endpoint(Ipv4Addr::new(224, 0, 0, 251)))
            .await;
    }

    let mut query = [0u8; 512];
    loop {
        match socket.recv_from(&mut query).await {
            Ok((len, meta)) => {
                if len < 12 {
                    continue;
                }
                let src = match meta.endpoint.addr {
                    IpAddress::Ipv4(ip) => ip,
                    #[allow(unreachable_patterns)]
                    _ => continue,
                };
                if let Some(resp) = build_response(&query[..len], &octets, host) {
                    let _ = socket.send_to(&resp, mdns_endpoint(src)).await;
                }
            }
            Err(_) => Timer::after(Duration::from_millis(500)).await,
        }
    }
}

fn build_announcement(ip: &[u8; 4], host: &[u8]) -> Option<Vec<u8, 512>> {
    let mut r: Vec<u8, 512> = Vec::new();
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // ID
    r.extend_from_slice(&[0x84, 0x00]).ok()?; // QR=1, AA=1
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // QDCOUNT
    r.extend_from_slice(&[0x00, 0x01]).ok()?; // ANCOUNT
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // NSCOUNT
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // ARCOUNT
    append_a_record(&mut r, ip, host)?;
    Some(r)
}

/// Parse a query; if it asks for `<host>.local` A/ANY, build the A response.
fn build_response(query: &[u8], ip: &[u8; 4], host: &[u8]) -> Option<Vec<u8, 512>> {
    if query.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([query[2], query[3]]);
    if (flags >> 15) & 1 != 0 {
        return None; // a response, not a query
    }
    if u16::from_be_bytes([query[4], query[5]]) == 0 {
        return None; // no questions
    }

    // Walk the question's labels, matching <HOSTNAME>.local (case-insensitive).
    let expected: [&[u8]; 2] = [host, b"local"];
    let mut pos = 12;
    let mut idx = 0;
    let mut matches = true;
    while pos < query.len() {
        let n = query[pos] as usize;
        if n == 0 {
            pos += 1;
            break;
        }
        if n > 63 || pos + 1 + n > query.len() {
            return None;
        }
        let label = &query[pos + 1..pos + 1 + n];
        if idx >= expected.len()
            || label.len() != expected[idx].len()
            || !label
                .iter()
                .zip(expected[idx])
                .all(|(a, b)| a.eq_ignore_ascii_case(b))
        {
            matches = false;
        }
        idx += 1;
        pos += 1 + n;
    }
    if !matches || idx != expected.len() || pos + 4 > query.len() {
        return None;
    }

    let qtype = u16::from_be_bytes([query[pos], query[pos + 1]]);
    let qclass = u16::from_be_bytes([query[pos + 2], query[pos + 3]]);
    if (qtype != 1 && qtype != 255) || (qclass & 0x7FFF) != 1 {
        return None;
    }

    let mut r: Vec<u8, 512> = Vec::new();
    r.extend_from_slice(&query[0..2]).ok()?; // ID echoed
    r.extend_from_slice(&[0x84, 0x00]).ok()?; // QR=1, AA=1
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // QDCOUNT
    r.extend_from_slice(&[0x00, 0x01]).ok()?; // ANCOUNT
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // NSCOUNT
    r.extend_from_slice(&[0x00, 0x00]).ok()?; // ARCOUNT
    append_a_record(&mut r, ip, host)?;
    Some(r)
}

/// Append `<host>.local A IN <ip>` (cache-flush set) to `r`.
fn append_a_record(r: &mut Vec<u8, 512>, ip: &[u8; 4], host: &[u8]) -> Option<()> {
    r.push(host.len() as u8).ok()?;
    r.extend_from_slice(host).ok()?;
    r.push(5).ok()?;
    r.extend_from_slice(b"local").ok()?;
    r.push(0).ok()?;
    r.extend_from_slice(&[0x00, 0x01]).ok()?; // TYPE A
    r.extend_from_slice(&[0x80, 0x01]).ok()?; // CLASS IN + cache-flush
    r.extend_from_slice(&[0x00, 0x00, 0x00, 0x78]).ok()?; // TTL 120
    r.extend_from_slice(&[0x00, 0x04]).ok()?; // RDLENGTH
    r.extend_from_slice(ip).ok()?;
    Some(())
}
