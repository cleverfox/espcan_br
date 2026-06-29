//! WS2812B activity LED on GPIO4, driven directly via esp-hal's RMT peripheral.
//!
//! Green tracks inbound (bus->host) frames/s, red tracks outbound (host->bus), so
//! two-way traffic glows yellow. Driving RMT directly avoids the `esp-hal-smartled`
//! crate's strict esp-hal version pin.

use esp_hal::gpio::Level;
use esp_hal::rmt::{Channel, PulseCode, Tx};
use esp_hal::Blocking;

// --- Brightness mapping (tunable) -------------------------------------------

/// Per-second frame count that maps to full (capped) brightness.
pub const FRAMES_FOR_FULL: u32 = 50;
/// Maximum per-channel level — capped below 255 to stay eye-friendly / low-current.
pub const MAX_LEVEL: u8 = 64;
/// Floor applied when there is any activity, so a single frame is still visible.
pub const MIN_LEVEL: u8 = 4;

/// Map a frame count (over the last second) to a WS2812 channel level.
pub fn level_from_count(count: u32) -> u8 {
    if count == 0 {
        return 0;
    }
    let span = (MAX_LEVEL - MIN_LEVEL) as u32;
    let extra = count.saturating_mul(span) / FRAMES_FOR_FULL;
    (MIN_LEVEL as u32 + extra).min(MAX_LEVEL as u32) as u8
}

// --- WS2812B bit timing -----------------------------------------------------
// RMT source 80 MHz, clk_divider = 1 => 1 tick = 12.5 ns. Within datasheet
// tolerance; scope-verify on GPIO4 and tweak if colours look off.

const T0H: u16 = 32; // 0.40 us high for a '0' bit
const T0L: u16 = 68; // 0.85 us low
const T1H: u16 = 64; // 0.80 us high for a '1' bit
const T1L: u16 = 36; // 0.45 us low

/// 24 data bits + one end marker.
const LEN: usize = 25;

/// WS2812B activity LED bound to a configured RMT TX channel.
pub struct ActivityLed {
    ch: Option<Channel<'static, Blocking, Tx>>,
}

impl ActivityLed {
    pub fn new(ch: Channel<'static, Blocking, Tx>) -> Self {
        Self { ch: Some(ch) }
    }

    /// Set the LED colour (best-effort; the channel is always retained).
    pub fn set_rgb(&mut self, r: u8, g: u8, b: u8) {
        let Some(ch) = self.ch.take() else {
            return;
        };

        let mut data = [PulseCode::end_marker(); LEN];
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
        // data[24] stays end_marker() — terminates the transmission.

        // transmit() returns the channel back on error; wait() returns it back
        // (Ok or Err) too — so `self.ch` is always restored.
        self.ch = Some(match ch.transmit(&data) {
            Ok(txn) => match txn.wait() {
                Ok(c) => c,
                Err((_, c)) => c,
            },
            Err((_, c)) => c,
        });
    }
}
