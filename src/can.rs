//! Shared CAN (TWAI) manager and RX broadcast bus.
//!
//! A single [`CanManager`] owns the TWAI driver and is shared between every SLCAN
//! transport (UART, TCP) behind an async mutex. The controller is on-bus while any
//! transport has the channel open (`open_count > 0`). Received frames are published
//! to [`RX_BUS`]; each transport subscribes and emits frames while its own port is
//! open. TX from any open transport goes to the bus.

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::pubsub::{PubSubChannel, Subscriber};
use embassy_time::{Duration, Timer};
use embedded_can::{ExtendedId, Frame, Id, StandardId};
use esp_hal::twai::filter::SingleStandardFilter;
use esp_hal::twai::{BaudRate, EspTwaiFrame, TimingConfig, Twai, TwaiConfiguration, TwaiMode};
use esp_hal::Blocking;

use crate::slcan::CanFrame;

/// Default bitrate if a port opens without setting `Sn` first (S6 = 500 kbit).
pub const DEFAULT_BAUD: BaudRate = BaudRate::B500K;

/// RX broadcast bus. Capacity 16 frames, up to 4 concurrent subscribers
/// (UART + AP-TCP + STA-TCP + headroom), 1 publisher slot (we use an immediate
/// publisher, which is separate).
pub static RX_BUS: PubSubChannel<CriticalSectionRawMutex, CanFrame, 16, 4, 1> =
    PubSubChannel::new();

/// One subscriber to [`RX_BUS`].
pub type RxSub = Subscriber<'static, CriticalSectionRawMutex, CanFrame, 16, 4, 1>;

/// Grab a fresh RX subscriber (panics only if more than 4 are alive at once).
pub fn rx_subscriber() -> RxSub {
    RX_BUS.subscriber().expect("too many RX subscribers")
}

/// Cumulative CAN-bus frame counters (also sampled by the LED tick in `main`).
/// RX_COUNT = frames received from the bus, TX_COUNT = frames transmitted to it.
pub static RX_COUNT: AtomicU32 = AtomicU32::new(0);
pub static TX_COUNT: AtomicU32 = AtomicU32::new(0);

/// Per-transport frame counters (shown on the web page).
/// `*_IN` = frames the host sent us; `*_OUT` = bus frames we forwarded to the host.
pub static SER_IN: AtomicU32 = AtomicU32::new(0);
pub static SER_OUT: AtomicU32 = AtomicU32::new(0);
pub static TCP_IN: AtomicU32 = AtomicU32::new(0);
pub static TCP_OUT: AtomicU32 = AtomicU32::new(0);

/// The shared CAN controller.
pub static CAN: Mutex<CriticalSectionRawMutex, CanManager> = Mutex::new(CanManager::new());

pub struct CanManager {
    twai: Option<Twai<'static, Blocking>>,
    baud: BaudRate,
    open_count: u32,
}

impl CanManager {
    pub const fn new() -> Self {
        Self {
            twai: None,
            baud: DEFAULT_BAUD,
            open_count: 0,
        }
    }

    /// Set the bitrate from an SLCAN `Sn` digit. Only valid while fully closed.
    pub fn set_bitrate(&mut self, digit: u8) -> bool {
        if self.open_count != 0 {
            return false;
        }
        match baud_from_s(digit) {
            Some(b) => {
                self.baud = b;
                true
            }
            None => false,
        }
    }

    /// Activate a port. Starts the controller when the first port opens.
    pub fn open(&mut self, listen_only: bool) {
        if self.open_count == 0 {
            self.twai = Some(open_twai(self.baud, listen_only));
        }
        self.open_count += 1;
    }

    /// Deactivate a port. Stops the controller when the last port closes.
    pub fn close(&mut self) {
        if self.open_count > 0 {
            self.open_count -= 1;
            if self.open_count == 0 {
                self.twai = None; // drop -> controller off the bus
            }
        }
    }

    /// Transmit a frame to the bus (only if the channel is open).
    pub fn transmit(&mut self, frame: &CanFrame) -> bool {
        let Some(twai) = self.twai.as_mut() else {
            return false;
        };
        let Some(esp) = to_esp(frame) else {
            return false;
        };
        let ok = try_transmit(twai, &esp);
        if ok {
            TX_COUNT.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    /// Lawicel `F` status byte derived from the live TWAI error state.
    /// bit2 = error warning, bit5 = error passive, bit7 = bus error/bus-off.
    /// Returns 0 when the channel is closed.
    pub fn status_flags(&self) -> u8 {
        let Some(twai) = self.twai.as_ref() else {
            return 0;
        };
        let rec = twai.receive_error_count();
        let tec = twai.transmit_error_count();
        let mut f = 0u8;
        if rec >= 96 || tec >= 96 {
            f |= 1 << 2; // EI: error warning
        }
        if rec >= 128 || tec >= 128 {
            f |= 1 << 5; // EPI: error passive
        }
        if twai.is_bus_off() {
            f |= 1 << 7; // BEI: bus error / bus-off
        }
        f
    }

    /// Non-blocking drain of one received frame.
    pub fn poll_rx(&mut self) -> Option<CanFrame> {
        let twai = self.twai.as_mut()?;
        match twai.receive() {
            Ok(f) => {
                RX_COUNT.fetch_add(1, Ordering::Relaxed);
                Some(from_esp(&f))
            }
            Err(_) => None,
        }
    }
}

/// Poll the controller and broadcast received frames to all subscribers.
#[embassy_executor::task]
pub async fn can_rx_task() {
    let publisher = RX_BUS.immediate_publisher();
    loop {
        let mut got = false;
        {
            let mut m = CAN.lock().await;
            for _ in 0..16 {
                match m.poll_rx() {
                    Some(f) => {
                        publisher.publish_immediate(f);
                        got = true;
                    }
                    None => break,
                }
            }
        }
        if !got {
            Timer::after(Duration::from_millis(1)).await;
        }
    }
}

/// SLCAN `Sn` digit -> TWAI [`BaudRate`] (80 MHz TWAI clock). S0/S1 unsupported.
fn baud_from_s(digit: u8) -> Option<BaudRate> {
    Some(match digit {
        b'2' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 80,
            sync_jump_width: 3,
            tseg_1: 15,
            tseg_2: 4,
            triple_sample: false,
        }), // 50k
        b'3' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 40,
            sync_jump_width: 3,
            tseg_1: 15,
            tseg_2: 4,
            triple_sample: false,
        }), // 100k
        b'4' => BaudRate::B125K,
        b'5' => BaudRate::B250K,
        b'6' => BaudRate::B500K,
        b'7' => BaudRate::Custom(TimingConfig {
            baud_rate_prescaler: 4,
            sync_jump_width: 3,
            tseg_1: 16,
            tseg_2: 8,
            triple_sample: false,
        }), // 800k
        b'8' => BaudRate::B1000K,
        _ => return None,
    })
}

/// (Re)open the TWAI controller. Re-`steal()`s the peripherals so the bitrate can
/// change between close/open cycles; sound because the previous `Twai` is always
/// dropped before this is called.
fn open_twai(baud: BaudRate, listen_only: bool) -> Twai<'static, Blocking> {
    let p = unsafe { esp_hal::peripherals::Peripherals::steal() };
    let mode = if listen_only {
        TwaiMode::ListenOnly
    } else {
        TwaiMode::Normal
    };
    // CAN RX = GPIO26, CAN TX = GPIO27 on this board.
    let mut cfg = TwaiConfiguration::new(p.TWAI0, p.GPIO26, p.GPIO27, baud, mode);
    cfg.set_filter(
        const { SingleStandardFilter::new(b"xxxxxxxxxxx", b"x", [b"xxxxxxxx", b"xxxxxxxx"]) },
    );
    cfg.start()
}

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

/// Bounded busy-wait transmit so a dead/unacked bus can't hang the caller.
///
/// This runs while the shared `CAN` async mutex is held, so it must not stall the
/// executor: a short retry budget covers transient FIFO-full backpressure and then
/// gives up (the host gets a BELL), rather than spinning on a wedged bus.
const TX_RETRIES: u32 = 50;

fn try_transmit(twai: &mut Twai<'static, Blocking>, frame: &EspTwaiFrame) -> bool {
    let mut tries = 0u32;
    loop {
        match twai.transmit(frame) {
            Ok(_) => return true,
            Err(nb::Error::WouldBlock) => {
                tries += 1;
                if tries > TX_RETRIES {
                    return false;
                }
            }
            Err(nb::Error::Other(_)) => return false,
        }
    }
}
