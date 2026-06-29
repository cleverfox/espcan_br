//! Minimal HTTP config server, served on both the AP and STA stacks.
//!
//! Phase 1: a status page plus `POST /api/wifi` to save station credentials.
//! UART-speed configuration is a planned addition.

use core::fmt::Write as _;
use core::sync::atomic::Ordering;

use alloc::string::String;
use embassy_net::tcp::TcpSocket;
use embedded_io_async::Write as _;
use embassy_net::IpListenEndpoint;
use embassy_time::{Duration, Timer};
use esp_storage::FlashStorage;

use crate::config::{self, AutoConnectConfig, DeviceConfig, WifiConfig};
use crate::{
    can, AUTOCONNECT_CONFIG, CONFIG_MODE, DEVICE_CONFIG, STA_IP, WIFI_CONFIG, WIFI_CONNECTED,
};

const HTTP_PORT: u16 = 80;

#[embassy_executor::task(pool_size = 2)]
pub async fn http_server(stack: embassy_net::Stack<'static>, name: &'static str) {
    let mut rx_buffer = [0u8; 2048];
    let mut tx_buffer = [0u8; 2048];

    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(30)));

        if let Err(e) = socket
            .accept(IpListenEndpoint { addr: None, port: HTTP_PORT })
            .await
        {
            log::warn!("[{}] HTTP accept error: {:?}", name, e);
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }

        let _ = handle_request(&mut socket).await;
        socket.close();
        Timer::after(Duration::from_millis(50)).await;
        socket.abort();
    }
}

async fn handle_request(socket: &mut TcpSocket<'_>) -> Result<(), embassy_net::tcp::Error> {
    let mut buffer = [0u8; 1024];
    let mut pos = 0;

    loop {
        match socket.read(&mut buffer[pos..]).await {
            Ok(0) => return Ok(()),
            Ok(len) => {
                pos += len;
                let req = unsafe { core::str::from_utf8_unchecked(&buffer[..pos]) };
                if req.contains("\r\n\r\n") || pos >= buffer.len() {
                    break;
                }
            }
            Err(e) => return Err(e),
        }
    }

    let request = unsafe { core::str::from_utf8_unchecked(&buffer[..pos]) };
    let (method, path) = parse_request_line(request);

    let response = match (method, path) {
        ("GET", "/") => index_page().await,
        ("GET", "/api/status") => status_json().await,
        ("POST", "/api/wifi") => {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            save_wifi(body).await
        }
        ("POST", "/api/device") => {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            save_device(body).await
        }
        ("POST", "/api/autoconnect") => {
            let body = request.split("\r\n\r\n").nth(1).unwrap_or("");
            save_autoconnect(body).await
        }
        _ => String::from("HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
    };

    socket.write_all(response.as_bytes()).await?;
    socket.flush().await?;
    Ok(())
}

fn parse_request_line(request: &str) -> (&str, &str) {
    let line = request.lines().next().unwrap_or("");
    let mut parts = line.split_whitespace();
    (parts.next().unwrap_or(""), parts.next().unwrap_or("/"))
}

/// Polls `/api/status` and updates only the live fields, so editing a form on the
/// page is never interrupted by a reload. Closes the page (`</body></html>`).
const STATUS_SCRIPT: &str = "<script>\
async function upd(){try{\
const s=await(await fetch('/api/status')).json();\
const g=(i,v)=>{const e=document.getElementById(i);if(e)e.textContent=v;};\
g('mode',s.mode);g('wifi',s.wifi?'yes':'no');g('staip',s.sta_ip);g('ssid',s.ssid);\
g('can_rx',s.can_rx);g('can_tx',s.can_tx);g('ser_in',s.ser_in);g('ser_out',s.ser_out);\
g('tcp_in',s.tcp_in);g('tcp_out',s.tcp_out);\
g('acstatus',s.ac_status);g('srvkey',s.server_key||'(none yet)');\
}catch(e){}}\
setInterval(upd,2000);upd();\
</script></body></html>";

/// Escape a string for safe interpolation into a JSON string literal.
fn json_escape(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(o, "\\u{:04x}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

/// JSON snapshot of the live fields, polled by the page.
async fn status_json() -> String {
    use core::sync::atomic::Ordering::Relaxed;
    let mode = if CONFIG_MODE.load(Relaxed) {
        "Configuration (AP)"
    } else {
        "Station"
    };
    let wifi = WIFI_CONNECTED.load(Relaxed);
    let sta_ip = {
        let ip = STA_IP.lock().await;
        let mut s = String::new();
        match *ip {
            Some(addr) => {
                let _ = write!(s, "{}", addr);
            }
            None => s.push_str("not connected"),
        }
        s
    };
    let ssid = {
        let cfg = WIFI_CONFIG.lock().await;
        json_escape(cfg.ssid_str())
    };
    let ac_status = crate::autoconnect::state_str(crate::AUTOCONNECT_STATE.load(Relaxed));
    let server_key = match crate::SERVER_PUBKEY.lock(|c| c.get()) {
        Some(k) => {
            let mut s = String::new();
            for b in k {
                let _ = write!(s, "{:02X}", b);
            }
            s
        }
        None => String::new(),
    };

    let mut b = String::new();
    let _ = write!(
        b,
        "{{\"mode\":\"{}\",\"wifi\":{},\"sta_ip\":\"{}\",\"ssid\":\"{}\",\
         \"can_rx\":{},\"can_tx\":{},\"ser_in\":{},\"ser_out\":{},\"tcp_in\":{},\"tcp_out\":{},\
         \"ac_status\":\"{}\",\"server_key\":\"{}\"}}",
        mode,
        wifi,
        sta_ip,
        ssid,
        can::RX_COUNT.load(Relaxed),
        can::TX_COUNT.load(Relaxed),
        can::SER_IN.load(Relaxed),
        can::SER_OUT.load(Relaxed),
        can::TCP_IN.load(Relaxed),
        can::TCP_OUT.load(Relaxed),
        ac_status,
        server_key,
    );

    let mut resp = String::new();
    let _ = write!(
        resp,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        b.len(),
        b,
    );
    resp
}

async fn index_page() -> String {
    let mode = if CONFIG_MODE.load(Ordering::Relaxed) {
        "Configuration (AP)"
    } else {
        "Station"
    };
    let wifi_s = if WIFI_CONNECTED.load(Ordering::Relaxed) {
        "yes"
    } else {
        "no"
    };
    let sta_ip = {
        let ip = STA_IP.lock().await;
        match *ip {
            Some(addr) => {
                let mut s = String::new();
                let _ = write!(s, "{}", addr);
                s
            }
            None => String::from("not connected"),
        }
    };
    let ssid = {
        let cfg = WIFI_CONFIG.lock().await;
        let mut s = String::new();
        let _ = write!(s, "{}", cfg.ssid_str());
        s
    };
    // Escape user-controlled values reflected into the page (stored XSS guard).
    let ssid = html_escape(&ssid);

    use core::sync::atomic::Ordering::Relaxed;
    let can_rx = can::RX_COUNT.load(Relaxed);
    let can_tx = can::TX_COUNT.load(Relaxed);
    let ser_in = can::SER_IN.load(Relaxed);
    let ser_out = can::SER_OUT.load(Relaxed);
    let tcp_in = can::TCP_IN.load(Relaxed);
    let tcp_out = can::TCP_OUT.load(Relaxed);

    let (dev_name, dev_baud) = {
        let d = DEVICE_CONFIG.lock().await;
        let mut s = String::new();
        let _ = write!(s, "{}", d.name_str());
        (s, d.baud())
    };
    let dev_name = html_escape(&dev_name);

    let (ac_en, ac_url) = {
        let a = AUTOCONNECT_CONFIG.lock().await;
        let mut s = String::new();
        let _ = write!(s, "{}", a.url_str());
        (a.enable_set(), html_escape(&s))
    };
    let ac_en_chk = if ac_en { "checked" } else { "" };
    let ac_status = crate::autoconnect::state_str(crate::AUTOCONNECT_STATE.load(Relaxed));
    let server_key_hex = match crate::SERVER_PUBKEY.lock(|c| c.get()) {
        Some(k) => {
            let mut s = String::new();
            for b in k {
                let _ = write!(s, "{:02X}", b);
            }
            s
        }
        None => String::from("(none yet)"),
    };

    let mut body = String::new();
    let _ = write!(
        body,
        "<!doctype html><html><head><meta name=viewport content=\"width=device-width,initial-scale=1\">\
         <title>espcan_br</title></head><body style=\"font-family:sans-serif;max-width:30em;margin:2em auto\">\
         <h2>espcan_br CAN bridge</h2>\
         <p>Mode: <b id=mode>{}</b><br>WiFi connected: <b id=wifi>{}</b><br>\
         STA IP: <b id=staip>{}</b><br>SSID: <b id=ssid>{}</b></p>\
         <h3>Frame counters</h3>\
         <table border=1 cellpadding=4 style=border-collapse:collapse>\
         <tr><th>Interface</th><th>in</th><th>out</th></tr>\
         <tr><td>CAN bus</td><td id=can_rx>{}</td><td id=can_tx>{}</td></tr>\
         <tr><td>Serial (USB)</td><td id=ser_in>{}</td><td id=ser_out>{}</td></tr>\
         <tr><td>TCP</td><td id=tcp_in>{}</td><td id=tcp_out>{}</td></tr>\
         </table>\
         <p style=color:#888>CAN in = received from bus, out = transmitted to bus; \
         serial/TCP in = host&rarr;adapter, out = adapter&rarr;host.</p>\
         <h3>Device</h3>\
         <form method=POST action=/api/device>\
         Name (mDNS <i>{}.local</i> / AP SSID):<br>\
         <input name=name maxlength=32 value=\"{}\" style=width:100%><br>\
         UART (USB) baud:<br>\
         <input name=baud type=number value=\"{}\" style=width:100%><br><br>\
         <button type=submit>Save &amp; reboot</button></form>\
         <h3>WiFi setup</h3>\
         <form method=POST action=/api/wifi>\
         SSID:<br><input name=ssid maxlength=32 style=width:100%><br>\
         Password:<br><input name=password type=password maxlength=64 style=width:100%><br><br>\
         <button type=submit>Save &amp; reboot</button></form>\
         <h3>Auto-connect (outbound)</h3>\
         <p>Status: <b id=acstatus>{}</b></p>\
         <p style=font-size:smaller>Server key (last TLS connection):<br>\
         <code id=srvkey style=word-break:break-all>{}</code></p>\
         <form method=POST action=/api/autoconnect>\
         <label><input type=checkbox name=enable {}> Enable (dial out, works behind NAT)</label><br>\
         URL:<br><input name=url maxlength=256 value=\"{}\" \
         placeholder=\"tls://host:port/?pubkey=..&amp;token=..\" style=width:100%><br>\
         <small style=color:#888>tcp:// or tls://; optional <b>pubkey</b> (pin server key) \
         and <b>token</b> (auth).</small>\
         <br><br><button type=submit>Save &amp; reboot</button></form>",
        mode, wifi_s, sta_ip, ssid,
        can_rx, can_tx, ser_in, ser_out, tcp_in, tcp_out,
        dev_name, dev_name, dev_baud,
        ac_status, server_key_hex, ac_en_chk, ac_url,
    );
    // Live fields refresh via JS (so editing a form is not interrupted by a reload).
    body.push_str(STATUS_SCRIPT);

    let mut resp = String::new();
    let _ = write!(
        resp,
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body,
    );
    resp
}

async fn save_wifi(body: &str) -> String {
    let mut ssid_buf = [0u8; 32];
    let mut pass_buf = [0u8; 64];
    let ssid_len = find_form_value(body, "ssid")
        .map(|v| url_decode_into(v, &mut ssid_buf))
        .unwrap_or(0);
    let pass_len = find_form_value(body, "password")
        .map(|v| url_decode_into(v, &mut pass_buf))
        .unwrap_or(0);

    if ssid_len == 0 {
        return String::from("HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n");
    }

    let ssid = unsafe { core::str::from_utf8_unchecked(&ssid_buf[..ssid_len]) };
    let pass = unsafe { core::str::from_utf8_unchecked(&pass_buf[..pass_len]) };
    let mut cfg = WifiConfig::new();
    cfg.set_credentials(ssid, pass);

    let mut flash = FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
    let saved = config::save(&mut flash, &cfg).is_ok();
    {
        *WIFI_CONFIG.lock().await = cfg;
    }

    if saved {
        log::info!("WiFi config saved: SSID='{}'", ssid);
        // Reboot shortly so STA mode picks up the new credentials.
        Timer::after(Duration::from_millis(800)).await;
        esp_hal::system::software_reset();
    }
    String::from(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 12\r\n\r\nsave failed\n",
    )
}

async fn save_device(body: &str) -> String {
    let mut name_buf = [0u8; 32];
    let name_len = find_form_value(body, "name")
        .map(|v| url_decode_into(v, &mut name_buf))
        .unwrap_or(0);
    // The name becomes a DNS label and the AP SSID, so restrict it to a safe
    // charset (also closes off any HTML/script injection at the source).
    let valid = name_len > 0
        && name_buf[..name_len]
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'-');
    if !valid {
        // body "bad name\n" = 9 bytes
        return String::from(
            "HTTP/1.1 400 Bad Request\r\nContent-Type: text/plain\r\nContent-Length: 9\r\n\r\nbad name\n",
        );
    }
    let name = unsafe { core::str::from_utf8_unchecked(&name_buf[..name_len]) };

    let mut baud_buf = [0u8; 16];
    let baud_len = find_form_value(body, "baud")
        .map(|v| url_decode_into(v, &mut baud_buf))
        .unwrap_or(0);
    let baud: u32 = core::str::from_utf8(&baud_buf[..baud_len])
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .filter(|&b| b > 0)
        .unwrap_or(config::DEFAULT_BAUD);

    let mut cfg = DeviceConfig::defaults();
    cfg.set(name, baud);

    let mut flash = FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
    let saved = config::save_device(&mut flash, &cfg).is_ok();
    {
        *DEVICE_CONFIG.lock().await = cfg;
    }

    if saved {
        log::info!("device config saved: name='{}' baud={}", name, baud);
        // Reboot so the new name (AP/mDNS) and UART baud take effect.
        Timer::after(Duration::from_millis(800)).await;
        esp_hal::system::software_reset();
    }
    String::from(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 12\r\n\r\nsave failed\n",
    )
}

/// Escape a string for safe interpolation into HTML text or a quoted attribute.
fn html_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

async fn save_autoconnect(body: &str) -> String {
    let enable = find_form_value(body, "enable").is_some();

    let mut url_buf = [0u8; config::URL_MAX];
    let url_len = find_form_value(body, "url")
        .map(|v| url_decode_into(v, &mut url_buf))
        .unwrap_or(0);
    let url = core::str::from_utf8(&url_buf[..url_len]).unwrap_or("");

    let mut cfg = AutoConnectConfig::new();
    cfg.set(enable, url);

    let mut flash = FlashStorage::new(unsafe { esp_hal::peripherals::FLASH::steal() });
    let saved = config::save_autoconnect(&mut flash, &cfg).is_ok();
    {
        *AUTOCONNECT_CONFIG.lock().await = cfg;
    }

    if saved {
        log::info!("autoconnect saved: en={} url={}", enable, url);
        Timer::after(Duration::from_millis(800)).await;
        esp_hal::system::software_reset();
    }
    String::from(
        "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 12\r\n\r\nsave failed\n",
    )
}

fn find_form_value<'a>(body: &'a str, key: &str) -> Option<&'a str> {
    body.split('&').find_map(|pair| {
        pair.split_once('=').and_then(|(k, v)| (k == key).then_some(v))
    })
}

fn url_decode_into(src: &str, dst: &mut [u8]) -> usize {
    let bytes = src.as_bytes();
    let (mut si, mut di) = (0, 0);
    while si < bytes.len() && di < dst.len() {
        if bytes[si] == b'%' && si + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[si + 1]), hex_nibble(bytes[si + 2])) {
                dst[di] = (h << 4) | l;
                di += 1;
                si += 3;
                continue;
            }
        }
        dst[di] = if bytes[si] == b'+' { b' ' } else { bytes[si] };
        di += 1;
        si += 1;
    }
    di
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}
