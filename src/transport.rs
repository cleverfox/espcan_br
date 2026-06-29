//! Generic SLCAN port runner, shared by the UART and TCP transports.
//!
//! Each port has its own line buffer, open flag and timestamp flag. It `select`s
//! between bytes arriving on its link and frames arriving on the shared RX bus,
//! parsing host commands via the pure [`crate::slcan`] code and emitting received
//! frames while the port is open. A read EOF/error (e.g. a closed TCP connection)
//! deactivates the port.

use core::sync::atomic::Ordering;

use embassy_futures::select::{select, Either};
use embedded_io_async::{Read, Write};

use crate::can::{self, RxSub, CAN};
use crate::slcan::{self, CanFrame, BELL, CR, MAX_FRAME_ASCII};

/// Which host-side interface a port belongs to (for the frame counters).
#[derive(Clone, Copy)]
pub enum Iface {
    Serial,
    Tcp,
}

impl Iface {
    fn count_in(self) {
        match self {
            Iface::Serial => &can::SER_IN,
            Iface::Tcp => &can::TCP_IN,
        }
        .fetch_add(1, Ordering::Relaxed);
    }

    fn count_out(self) {
        match self {
            Iface::Serial => &can::SER_OUT,
            Iface::Tcp => &can::TCP_OUT,
        }
        .fetch_add(1, Ordering::Relaxed);
    }
}

/// Outcome of one `select` round, captured so the borrow of `conn` from the read
/// future ends before we use `conn` for writes.
enum Act {
    Bytes(usize),
    Frame(CanFrame),
    Stop,
}

/// Run one SLCAN port over a single full-duplex connection (UART wrapper, plain
/// TCP socket, or a TLS connection) until the link reports EOF/error.
///
/// A single `conn` is used for both reading and writing: each round we `select`
/// between an inbound read and an RX-bus frame, capture an owned [`Act`] (dropping
/// the read future, which borrows `conn`), then act — writing via `conn`. The TLS
/// connection isn't `Clone`, so it can't be split; this single-object form works
/// for all transports. The read future is cancel-safe (embassy TCP + the TLS
/// record reader both resume partial reads).
pub async fn run_port<C: Read + Write>(conn: &mut C, sub: &mut RxSub, iface: Iface) {
    let mut line = [0u8; 64];
    let mut len = 0usize;
    let mut open = false;
    let mut timestamp = false;
    let mut rxbuf = [0u8; 64];

    loop {
        let act = match select(conn.read(&mut rxbuf), sub.next_message_pure()).await {
            Either::First(Ok(0)) | Either::First(Err(_)) => Act::Stop,
            Either::First(Ok(n)) => Act::Bytes(n),
            Either::Second(frame) => Act::Frame(frame),
        };

        match act {
            Act::Stop => break,
            Act::Bytes(n) => {
                let mut wrote = false;
                for i in 0..n {
                    match rxbuf[i] {
                        CR => {
                            if len > 0 {
                                dispatch(&line[..len], &mut open, &mut timestamp, conn, iface)
                                    .await;
                                len = 0;
                                wrote = true;
                            }
                        }
                        b'\n' => {}
                        b => {
                            if len < line.len() {
                                line[len] = b;
                                len += 1;
                            } else {
                                len = 0;
                                let _ = conn.write_all(&[BELL]).await;
                                wrote = true;
                            }
                        }
                    }
                }
                // TLS buffers writes until flushed; emit any command responses now.
                if wrote {
                    let _ = conn.flush().await;
                }
            }
            Act::Frame(frame) => {
                if open {
                    let mut out = [0u8; MAX_FRAME_ASCII];
                    let ts = if timestamp { Some(now_ms()) } else { None };
                    let nn = slcan::format_frame(&frame, ts, &mut out);
                    let _ = conn.write_all(&out[..nn]).await;
                    let _ = conn.flush().await; // emit the TLS record / push the frame out
                    iface.count_out();
                }
            }
        }
    }

    // Link gone: deactivate this port if it was open.
    if open {
        CAN.lock().await.close();
    }
}

async fn dispatch<W: Write>(
    line: &[u8],
    open: &mut bool,
    ts: &mut bool,
    writer: &mut W,
    iface: Iface,
) {
    let cmd = line[0];
    match cmd {
        b'V' => reply(writer, b"V1013\r").await,
        b'N' => reply(writer, b"N1234\r").await,
        b'F' => {
            let f = CAN.lock().await.status_flags();
            reply(writer, &[b'F', hex_nibble(f >> 4), hex_nibble(f & 0x0f), CR]).await;
        }
        // Acceptance code/mask (M/m) and auto-retransmit (A) are accepted as
        // no-ops for host tolerance; filtering is left to the host.
        b'M' | b'm' | b'A' => reply(writer, &[CR]).await,
        b'Z' => match line.get(1) {
            Some(b'0') => {
                *ts = false;
                reply(writer, &[CR]).await;
            }
            Some(b'1') => {
                *ts = true;
                reply(writer, &[CR]).await;
            }
            _ => reply(writer, &[BELL]).await,
        },
        b'S' => {
            let ok = match line.get(1).copied() {
                Some(d) => CAN.lock().await.set_bitrate(d),
                None => false,
            };
            reply(writer, if ok { &[CR] } else { &[BELL] }).await;
        }
        b's' => reply(writer, &[BELL]).await, // custom BTR: unsupported
        b'O' | b'L' => {
            if !*open {
                CAN.lock().await.open(cmd == b'L');
                *open = true;
            }
            reply(writer, &[CR]).await;
        }
        b'C' => {
            if *open {
                CAN.lock().await.close();
                *open = false;
            }
            reply(writer, &[CR]).await;
        }
        b't' | b'T' | b'r' | b'R' => {
            let frame = slcan::parse_tx(line);
            if frame.is_some() {
                iface.count_in(); // a valid frame arrived on this interface
            }
            if !*open {
                reply(writer, &[BELL]).await;
                return;
            }
            let ok = match frame {
                Some(f) => CAN.lock().await.transmit(&f),
                None => false,
            };
            if ok {
                // Lawicel transmit confirmation: 'z' for 11-bit, 'Z' for 29-bit.
                let ack = if cmd == b'T' || cmd == b'R' { b'Z' } else { b'z' };
                reply(writer, &[ack, CR]).await;
            } else {
                reply(writer, &[BELL]).await;
            }
        }
        _ => reply(writer, &[BELL]).await,
    }
}

async fn reply<W: Write>(writer: &mut W, bytes: &[u8]) {
    let _ = writer.write_all(bytes).await;
}

fn hex_nibble(n: u8) -> u8 {
    let n = n & 0x0f;
    if n < 10 {
        b'0' + n
    } else {
        b'A' + (n - 10)
    }
}

/// Current ms, wrapped to the Lawicel 0..=59999 timestamp range.
fn now_ms() -> u16 {
    (esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis()
        % 60_000) as u16
}
