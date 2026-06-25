//! WS2812B activity LED on GPIO4, driven directly via esp-hal's RMT peripheral.
//!
//! The board has a single WS2812B (GRB order). We use it as a bidirectional CAN
//! activity indicator: green brightness tracks inbound (bus→host) frames-per-second
//! and red tracks outbound (host→bus), so two-way traffic glows yellow.
//!
//! Driving the LED directly (instead of the `esp-hal-smartled` crate, which pins to
//! exactly esp-hal 1.0.0 stable) keeps us on our pinned `=1.0.0-beta.0` with no
//! extra dependency — RMT is part of esp-hal's `unstable` feature, already enabled.

use esp_hal::gpio::Level;
use esp_hal::rmt::{PulseCode, TxChannel};

// --- Brightness mapping (tunable) -------------------------------------------

/// Per-second frame count that maps to full (capped) brightness.
pub const FRAMES_FOR_FULL: u32 = 50;
/// Maximum per-channel level — capped well below 255 to stay eye-friendly and
/// keep WS2812 current draw low.
pub const MAX_LEVEL: u8 = 64;
/// Floor applied when there is any activity, so a single frame is still visible.
pub const MIN_LEVEL: u8 = 4;

/// Map a frame count (over the last second) to a WS2812 channel level.
///
/// Pure and hardware-agnostic: `0 -> off`, otherwise a linear ramp from
/// [`MIN_LEVEL`] up to [`MAX_LEVEL`] reached at [`FRAMES_FOR_FULL`] frames/s.
pub fn level_from_count(count: u32) -> u8 {
    if count == 0 {
        return 0;
    }
    let span = (MAX_LEVEL - MIN_LEVEL) as u32;
    let extra = count.saturating_mul(span) / FRAMES_FOR_FULL;
    (MIN_LEVEL as u32 + extra).min(MAX_LEVEL as u32) as u8
}

// --- WS2812B bit timing -----------------------------------------------------
//
// RMT source clock 80 MHz with clk_divider = 1 => 1 tick = 12.5 ns.
// WS2812B bit period ~1.25 us; values below are within datasheet tolerance.
// Scope-verify on GPIO4 and tweak if colours/brightness look off.

const T0H: u16 = 32; // 0.40 us high for a '0' bit
const T0L: u16 = 68; // 0.85 us low
const T1H: u16 = 64; // 0.80 us high for a '1' bit
const T1L: u16 = 36; // 0.45 us low

/// 24 data bits + one terminator (line idles low -> WS2812 reset latch).
const LEN: usize = 25;

/// WS2812B activity LED bound to a configured RMT TX channel.
///
/// Generic over the channel type so we don't have to name the concrete
/// `Channel<Blocking, N>` produced by `.configure()`.
pub struct ActivityLed<C: TxChannel> {
    ch: Option<C>,
}

impl<C: TxChannel> ActivityLed<C> {
    pub fn new(ch: C) -> Self {
        Self { ch: Some(ch) }
    }

    /// Set the LED colour. Best-effort: a (very unlikely) RMT error drops the
    /// channel and the LED simply stops updating rather than panicking.
    pub fn set_rgb(&mut self, r: u8, g: u8, b: u8) {
        let Some(ch) = self.ch.take() else {
            return;
        };

        let mut data = [PulseCode::empty(); LEN];
        let bytes = [g, r, b]; // WS2812B wants GRB order
        let mut i = 0;
        for byte in bytes {
            for bit in (0..8).rev() {
                data[i] = if (byte >> bit) & 1 != 0 {
                    PulseCode::new(Level::High, T1H, Level::Low, T1L)
                } else {
                    PulseCode::new(Level::High, T0H, Level::Low, T0L)
                };
                i += 1;
            }
        }
        // data[24] stays PulseCode::empty() — ends the transmission.

        if let Ok(tx) = ch.transmit(&data) {
            self.ch = tx.wait().ok();
        }
    }
}
