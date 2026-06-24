//! SLCAN / LAWICEL (CANUSB) ASCII protocol — pure, `no_std`, hardware-agnostic.
//!
//! This module only deals with bytes and a neutral [`CanFrame`] view. The glue in
//! `main.rs` converts between [`CanFrame`] and the esp-hal `EspTwaiFrame`, so this
//! code could be unit-tested on a host with no ESP toolchain.
//!
//! Reference: the de-facto Lawicel/CANUSB serial line protocol spoken by Linux
//! `slcand`/`can-utils`.

/// ASCII BEL — the SLCAN "error / NACK" response.
pub const BELL: u8 = 0x07;
/// ASCII CR — the SLCAN command terminator and "OK / ACK" response.
pub const CR: u8 = b'\r';

/// Maximum length of a formatted received frame:
/// `T` + 8 id + 1 dlc + 16 data + 4 timestamp + CR = 30, rounded up.
pub const MAX_FRAME_ASCII: usize = 32;

/// A neutral CAN frame, independent of any HAL.
#[derive(Clone, Copy, Default)]
pub struct CanFrame {
    /// 11-bit (standard) or 29-bit (extended) identifier.
    pub id: u32,
    /// Extended (29-bit) frame if true, standard (11-bit) if false.
    pub ext: bool,
    /// Remote-transmission-request frame (no data payload).
    pub rtr: bool,
    /// Data length code, 0..=8.
    pub dlc: u8,
    /// Payload; only the first `dlc` bytes are meaningful.
    pub data: [u8; 8],
}

#[inline]
fn nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn hex_digit(v: u8) -> u8 {
    let v = v & 0x0f;
    if v < 10 {
        b'0' + v
    } else {
        b'A' + (v - 10)
    }
}

/// Parse `n` hex chars into a `u32` (big-endian nibbles).
fn parse_hex(bytes: &[u8]) -> Option<u32> {
    let mut v = 0u32;
    for &b in bytes {
        v = (v << 4) | nibble(b)? as u32;
    }
    Some(v)
}

/// Parse a transmit command line: `t`/`T`/`r`/`R` followed by id, dlc and data.
///
/// * `tIIILDD..`   standard data frame   (3 hex id)
/// * `TIIIIIIIILDD..` extended data frame (8 hex id)
/// * `rIIIL`       standard remote frame
/// * `RIIIIIIIIL`  extended remote frame
///
/// Returns `None` on any malformed input.
pub fn parse_tx(line: &[u8]) -> Option<CanFrame> {
    let mut f = CanFrame::default();
    let (ext, rtr, idlen) = match line.first()? {
        b't' => (false, false, 3usize),
        b'T' => (true, false, 8usize),
        b'r' => (false, true, 3usize),
        b'R' => (true, true, 8usize),
        _ => return None,
    };
    f.ext = ext;
    f.rtr = rtr;

    // id field
    let mut i = 1usize;
    if line.len() < i + idlen + 1 {
        return None;
    }
    f.id = parse_hex(&line[i..i + idlen])?;
    i += idlen;

    // dlc
    let dlc = nibble(line[i])?;
    if dlc > 8 {
        return None;
    }
    f.dlc = dlc;
    i += 1;

    // data (data frames only)
    if !rtr {
        if line.len() < i + (dlc as usize) * 2 {
            return None;
        }
        for k in 0..dlc as usize {
            let hi = nibble(line[i])?;
            let lo = nibble(line[i + 1])?;
            f.data[k] = (hi << 4) | lo;
            i += 2;
        }
    }
    Some(f)
}

/// Format a received frame as an SLCAN line into `out`, returning its length.
///
/// `out` must be at least [`MAX_FRAME_ASCII`] bytes. If `timestamp` is `Some`, a
/// 4-hex-digit millisecond timestamp (0..=59999, per Lawicel) is appended before
/// the trailing CR.
pub fn format_frame(f: &CanFrame, timestamp: Option<u16>, out: &mut [u8]) -> usize {
    let mut n = 0usize;

    out[n] = match (f.ext, f.rtr) {
        (false, false) => b't',
        (true, false) => b'T',
        (false, true) => b'r',
        (true, true) => b'R',
    };
    n += 1;

    if f.ext {
        for shift in [28, 24, 20, 16, 12, 8, 4, 0] {
            out[n] = hex_digit((f.id >> shift) as u8);
            n += 1;
        }
    } else {
        for shift in [8, 4, 0] {
            out[n] = hex_digit((f.id >> shift) as u8);
            n += 1;
        }
    }

    out[n] = hex_digit(f.dlc);
    n += 1;

    if !f.rtr {
        for k in 0..f.dlc as usize {
            out[n] = hex_digit(f.data[k] >> 4);
            n += 1;
            out[n] = hex_digit(f.data[k]);
            n += 1;
        }
    }

    if let Some(ts) = timestamp {
        for shift in [12, 8, 4, 0] {
            out[n] = hex_digit((ts >> shift) as u8);
            n += 1;
        }
    }

    out[n] = CR;
    n += 1;
    n
}
