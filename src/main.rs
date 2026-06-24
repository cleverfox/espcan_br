//! SLCAN (LAWICEL / CANUSB) bridge for the WeAct CAN485 ESP32 dev board.
//!
//! Exposes the classic ESP32's TWAI (CAN) controller as a serial-line CAN adapter
//! over the onboard USB-UART (UART0), speaking the same ASCII protocol that Linux
//! `slcand`/`can-utils` use. This is a Rust `no_std` port of the older STM32
//! `uart_can_br` firmware.
//!
//! Board pinout (WeAct CAN485DevBoardV1_ESP32):
//!   * CAN RX  = GPIO26, CAN TX = GPIO27  (to the onboard CAN transceiver)
//!   * UART0   = GPIO3 (RX) / GPIO1 (TX)  (to the onboard USB-UART bridge)
//!
//! Host usage (Linux):
//!   slcand -o -s6 -t hw -S 115200 /dev/ttyUSB0 can0 && ip link set up can0
//!   (-s6 = 500 kbit; see the bitrate table below.)

#![no_std]
#![no_main]

mod slcan;

use embedded_can::{ExtendedId, Frame, Id, StandardId};
use esp_backtrace as _;
use esp_hal::twai::filter::SingleStandardFilter;
use esp_hal::twai::{BaudRate, EspTwaiFrame, TimingConfig, Twai, TwaiConfiguration, TwaiMode};
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_hal::Blocking;

use slcan::{CanFrame, BELL, CR, MAX_FRAME_ASCII};

/// Host serial speed of the USB-UART link. Must match the `-S` value given to
/// `slcand`. 115200 is safe everywhere; raise it (e.g. 1_000_000) for more CAN
/// throughput once you have confirmed the host keeps up.
const SLCAN_UART_BAUD: u32 = 115_200;

/// Default CAN bitrate used if the host opens the channel without sending `Sn`
/// first. S6 = 500 kbit.
const DEFAULT_BAUD: BaudRate = BaudRate::B500K;

/// Map an SLCAN `Sn` digit to a TWAI [`BaudRate`].
///
/// The ESP32 TWAI clock is 80 MHz; custom entries are the ESP-IDF-proven timings.
/// S0 (10 kbit) and S1 (20 kbit) need a prescaler beyond the classic ESP32's
/// hardware range, so they are unsupported and return `None`.
fn baud_from_s(digit: u8) -> Option<BaudRate> {
    Some(match digit {
        // S2 = 50 kbit:  80e6 / (80 * (1+15+4)) = 50000
        b'2' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 80,
            sync_jump_width: 3,
            tseg_1: 15,
            tseg_2: 4,
            triple_sample: false,
        }),
        // S3 = 100 kbit: 80e6 / (40 * 20) = 100000
        b'3' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 40,
            sync_jump_width: 3,
            tseg_1: 15,
            tseg_2: 4,
            triple_sample: false,
        }),
        b'4' => BaudRate::B125K,
        b'5' => BaudRate::B250K,
        b'6' => BaudRate::B500K,
        // S7 = 800 kbit: 80e6 / (4 * (1+16+8)) = 800000
        b'7' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 4,
            sync_jump_width: 3,
            tseg_1: 16,
            tseg_2: 8,
            triple_sample: false,
        }),
        b'8' => BaudRate::B1000K,
        _ => return None, // S0 (10k) / S1 (20k): not representable on classic ESP32
    })
}

/// Current ms-resolution timestamp, wrapped to the Lawicel 0..=59999 range.
///
/// NOTE: `esp_hal::time` is the most version-volatile API used here — if you bump
/// esp-hal and this fails to compile, adjust just this function.
fn now_ms() -> u16 {
    let ms = esp_hal::time::Instant::now()
        .duration_since_epoch()
        .as_millis();
    (ms % 60_000) as u16
}

/// Convert a neutral [`CanFrame`] into an `EspTwaiFrame` for transmission.
fn to_esp(f: &CanFrame) -> Option<EspTwaiFrame> {
    let id: Id = if f.ext {
        ExtendedId::new(f.id)?.into()
    } else {
        StandardId::new(f.id as u16)?.into()
    };
    if f.rtr {
        EspTwaiFrame::new_remote(id, f.dlc as usize)
    } else {
        EspTwaiFrame::new(id, &f.data[..f.dlc as usize])
    }
}

/// Convert a received `EspTwaiFrame` into a neutral [`CanFrame`].
fn from_esp(frame: &EspTwaiFrame) -> CanFrame {
    let mut f = CanFrame::default();
    match frame.id() {
        Id::Standard(s) => {
            f.ext = false;
            f.id = s.as_raw() as u32;
        }
        Id::Extended(e) => {
            f.ext = true;
            f.id = e.as_raw();
        }
    }
    f.rtr = frame.is_remote_frame();
    f.dlc = frame.dlc() as u8;
    let d = frame.data();
    f.data[..d.len()].copy_from_slice(d);
    f
}

/// (Re)open the TWAI controller at `baud`.
///
/// We re-`steal()` the peripherals each time the channel is opened so the bitrate
/// can change between `C`/`O` cycles. This is sound because the bridge is a single
/// blocking superloop and the previous `Twai` is always dropped (releasing TWAI0
/// and the pins) before this is called.
fn open_twai(baud: BaudRate, listen_only: bool) -> Twai<'static, Blocking> {
    let p = unsafe { esp_hal::peripherals::Peripherals::steal() };
    let mode = if listen_only {
        TwaiMode::ListenOnly
    } else {
        TwaiMode::Normal
    };
    // CAN RX = GPIO26, CAN TX = GPIO27 on this board.
    let mut cfg = TwaiConfiguration::new(p.TWAI0, p.GPIO26, p.GPIO27, baud, mode);
    // Accept everything (all bits don't-care) — filtering is the host's job.
    cfg.set_filter(
        const { SingleStandardFilter::new(b"xxxxxxxxxxx", b"x", [b"xxxxxxxx", b"xxxxxxxx"]) },
    );
    cfg.start()
}

/// The bridge state machine.
struct Bridge {
    line: [u8; 64],
    len: usize,
    baud: BaudRate,
    timestamp: bool,
    can: Option<Twai<'static, Blocking>>,
}

impl Bridge {
    fn new() -> Self {
        Self {
            line: [0; 64],
            len: 0,
            baud: DEFAULT_BAUD,
            timestamp: false,
            can: None,
        }
    }

    /// Feed one received host byte; dispatch the command on CR.
    fn feed_byte(&mut self, b: u8, uart: &mut Uart<'static, Blocking>) {
        match b {
            CR => {
                if self.len > 0 {
                    let len = self.len;
                    self.len = 0;
                    self.handle_line(len, uart);
                } else {
                    // Bare CR — Lawicel adapters answer with CR (keeps hosts happy).
                    reply(uart, &[CR]);
                }
            }
            b'\n' => { /* ignore LF */ }
            _ => {
                if self.len < self.line.len() {
                    self.line[self.len] = b;
                    self.len += 1;
                } else {
                    // Overflow — drop the partial line and signal an error.
                    self.len = 0;
                    reply(uart, &[BELL]);
                }
            }
        }
    }

    fn handle_line(&mut self, len: usize, uart: &mut Uart<'static, Blocking>) {
        let cmd = self.line[0];
        let line = &self.line[..len];

        match cmd {
            b'V' => reply(uart, b"V1013\r"),
            b'N' => reply(uart, b"N1234\r"),
            b'F' => reply(uart, b"F00\r"), // status flags: nothing latched
            b'M' | b'm' => reply(uart, &[CR]), // accept code/mask: we accept all, no-op
            b'Z' => {
                // Z0 / Z1 — timestamp off / on.
                match line.get(1) {
                    Some(b'0') => {
                        self.timestamp = false;
                        reply(uart, &[CR]);
                    }
                    Some(b'1') => {
                        self.timestamp = true;
                        reply(uart, &[CR]);
                    }
                    _ => reply(uart, &[BELL]),
                }
            }
            b'S' => {
                // Set bitrate — only valid while the channel is closed.
                if self.can.is_some() {
                    reply(uart, &[BELL]);
                } else if let Some(b) = line.get(1).copied().and_then(baud_from_s) {
                    self.baud = b;
                    reply(uart, &[CR]);
                } else {
                    reply(uart, &[BELL]);
                }
            }
            b's' => reply(uart, &[BELL]), // custom BTR registers: unsupported
            b'O' | b'L' => {
                // Open normal (O) or listen-only (L).
                if self.can.is_none() {
                    self.can = Some(open_twai(self.baud, cmd == b'L'));
                }
                reply(uart, &[CR]);
            }
            b'C' => {
                // Close — drop the driver, which releases the bus.
                self.can = None;
                reply(uart, &[CR]);
            }
            b't' | b'T' | b'r' | b'R' => {
                // Parse before the mutable call so `line` (a borrow of `self`) is
                // released first. `CanFrame` is `Copy`, so this carries no borrow.
                let parsed = slcan::parse_tx(line);
                self.handle_tx(parsed, uart);
            }
            _ => reply(uart, &[BELL]),
        }
    }

    fn handle_tx(&mut self, parsed: Option<CanFrame>, uart: &mut Uart<'static, Blocking>) {
        let Some(twai) = self.can.as_mut() else {
            reply(uart, &[BELL]); // channel not open
            return;
        };
        let ok = parsed
            .and_then(|f| to_esp(&f))
            .map(|frame| try_transmit(twai, &frame))
            .unwrap_or(false);
        reply(uart, if ok { &[CR] } else { &[BELL] });
    }

    /// Drain pending received CAN frames to the host (bounded per call so the UART
    /// read side stays responsive).
    fn pump_can(&mut self, uart: &mut Uart<'static, Blocking>) {
        let Some(twai) = self.can.as_mut() else {
            return;
        };
        let mut out = [0u8; MAX_FRAME_ASCII];
        for _ in 0..16 {
            match twai.receive() {
                Ok(frame) => {
                    let f = from_esp(&frame);
                    let ts = if self.timestamp { Some(now_ms()) } else { None };
                    let n = slcan::format_frame(&f, ts, &mut out);
                    reply(uart, &out[..n]);
                }
                Err(nb::Error::WouldBlock) => break,
                Err(nb::Error::Other(_)) => break, // bus error; alerts are not surfaced
            }
        }
    }
}

/// Attempt a transmit with a bounded busy-wait so a dead/unacked bus cannot hang
/// the whole bridge.
fn try_transmit(twai: &mut Twai<'static, Blocking>, frame: &EspTwaiFrame) -> bool {
    let mut tries = 0u32;
    loop {
        match twai.transmit(frame) {
            Ok(_) => return true,
            Err(nb::Error::WouldBlock) => {
                tries += 1;
                if tries > 200_000 {
                    return false;
                }
            }
            Err(nb::Error::Other(_)) => return false,
        }
    }
}

/// Write bytes to the host, ignoring transient TX errors.
fn reply(uart: &mut Uart<'static, Blocking>, bytes: &[u8]) {
    let _ = uart.write(bytes);
}

#[esp_hal::main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // SLCAN host link over the onboard USB-UART bridge (UART0: GPIO3 RX / GPIO1 TX).
    let mut uart = Uart::new(
        peripherals.UART0,
        UartConfig::default().with_baudrate(SLCAN_UART_BAUD),
    )
    .expect("UART0 init")
    .with_rx(peripherals.GPIO3)
    .with_tx(peripherals.GPIO1);

    let mut bridge = Bridge::new();
    let mut rxbuf = [0u8; 64];

    loop {
        // 1) Host -> command parser.
        if let Ok(n) = uart.read(&mut rxbuf) {
            for i in 0..n {
                bridge.feed_byte(rxbuf[i], &mut uart);
            }
        }
        // 2) CAN bus -> host.
        bridge.pump_can(&mut uart);
    }
}
