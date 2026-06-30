//! Persistent configuration in the NVS data partition.
//!
//! Pattern follows the esp-hal reference projects: locate the NVS partition via
//! the partition table, then read/write fixed-offset fixed-size structs.

use embedded_storage::{ReadStorage, Storage};
use esp_bootloader_esp_idf::partitions;
use esp_hal::rng::Rng;
use esp_storage::FlashStorage;
use sha2::{Digest, Sha256};

/// Default device name (used for the AP SSID and the mDNS `<name>.local`).
pub const DEFAULT_NAME: &str = "espcan-br";
/// Default UART (USB) baud rate.
pub const DEFAULT_BAUD: u32 = 115_200;

const WIFI_CONFIG_MAGIC: u32 = 0xC0F1_6001;
const WIFI_CONFIG_OFFSET: u32 = 0;

const DEVICE_CONFIG_MAGIC: u32 = 0xDE71_CE01;
const DEVICE_CONFIG_OFFSET: u32 = 128;

const AUTOCONN_CONFIG_MAGIC: u32 = 0xAC0C_0002;
const AUTOCONN_CONFIG_OFFSET: u32 = 256;
/// Max outbound URL length (tls://host:port/?pubkey=..&token=..).
pub const URL_MAX: usize = 256;

const AUTH_CONFIG_MAGIC: u32 = 0xA071_0101;
// AutoConnectConfig is saved in a 384-byte aligned slot at offset 256 (256..640),
// so the web-login credentials live at 640.
const AUTH_CONFIG_OFFSET: u32 = 640;

/// WiFi station credentials, persisted to flash.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct WifiConfig {
    magic: u32,
    ssid: [u8; 32],
    ssid_len: u8,
    password: [u8; 64],
    password_len: u8,
    _padding: [u8; 2],
}

impl WifiConfig {
    pub const fn new() -> Self {
        Self {
            magic: 0,
            ssid: [0; 32],
            ssid_len: 0,
            password: [0; 64],
            password_len: 0,
            _padding: [0; 2],
        }
    }

    pub fn is_valid(&self) -> bool {
        self.magic == WIFI_CONFIG_MAGIC && self.ssid_len > 0
    }

    pub fn ssid_str(&self) -> &str {
        if self.ssid_len == 0 {
            ""
        } else {
            unsafe { core::str::from_utf8_unchecked(&self.ssid[..self.ssid_len as usize]) }
        }
    }

    pub fn password_str(&self) -> &str {
        if self.password_len == 0 {
            ""
        } else {
            unsafe { core::str::from_utf8_unchecked(&self.password[..self.password_len as usize]) }
        }
    }

    pub fn set_credentials(&mut self, ssid: &str, password: &str) {
        self.magic = WIFI_CONFIG_MAGIC;
        self.ssid_len = ssid.len().min(32) as u8;
        self.ssid[..self.ssid_len as usize]
            .copy_from_slice(&ssid.as_bytes()[..self.ssid_len as usize]);
        self.password_len = password.len().min(64) as u8;
        self.password[..self.password_len as usize]
            .copy_from_slice(&password.as_bytes()[..self.password_len as usize]);
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let config = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) };
        if config.is_valid() { Some(config) } else { None }
    }
}

pub fn load(flash: &mut FlashStorage) -> Option<WifiConfig> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).ok()?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .ok()??;
    let mut part = nvs.as_embedded_storage(flash);
    let mut buf = [0u8; 128];
    part.read(WIFI_CONFIG_OFFSET, &mut buf).ok()?;
    WifiConfig::from_bytes(&buf)
}

pub fn save(flash: &mut FlashStorage, config: &WifiConfig) -> Result<(), ()> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).map_err(|_| ())?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .map_err(|_| ())?
        .ok_or(())?;
    let mut part = nvs.as_embedded_storage(flash);
    let bytes = config.as_bytes();
    let mut aligned = [0u8; 128];
    aligned[..bytes.len()].copy_from_slice(bytes);
    part.write(WIFI_CONFIG_OFFSET, &aligned).map_err(|_| ())
}

/// Device name (mDNS/AP SSID) and UART baud, persisted to flash.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct DeviceConfig {
    magic: u32,
    name: [u8; 32],
    name_len: u8,
    _pad: [u8; 3],
    baud: u32,
}

impl DeviceConfig {
    pub const fn new() -> Self {
        Self {
            magic: 0,
            name: [0; 32],
            name_len: 0,
            _pad: [0; 3],
            baud: DEFAULT_BAUD,
        }
    }

    /// Built-in defaults (used when nothing is stored in flash).
    pub fn defaults() -> Self {
        let mut c = Self::new();
        c.set(DEFAULT_NAME, DEFAULT_BAUD);
        c
    }

    fn is_valid(&self) -> bool {
        self.magic == DEVICE_CONFIG_MAGIC && self.name_len > 0
    }

    pub fn name_str(&self) -> &str {
        if self.name_len == 0 {
            DEFAULT_NAME
        } else {
            unsafe { core::str::from_utf8_unchecked(&self.name[..self.name_len as usize]) }
        }
    }

    pub fn baud(&self) -> u32 {
        if self.baud == 0 {
            DEFAULT_BAUD
        } else {
            self.baud
        }
    }

    pub fn set(&mut self, name: &str, baud: u32) {
        self.magic = DEVICE_CONFIG_MAGIC;
        self.name_len = name.len().min(32) as u8;
        self.name[..self.name_len as usize]
            .copy_from_slice(&name.as_bytes()[..self.name_len as usize]);
        self.baud = baud;
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let config = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) };
        if config.is_valid() { Some(config) } else { None }
    }
}

pub fn load_device(flash: &mut FlashStorage) -> Option<DeviceConfig> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).ok()?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .ok()??;
    let mut part = nvs.as_embedded_storage(flash);
    let mut buf = [0u8; 64];
    part.read(DEVICE_CONFIG_OFFSET, &mut buf).ok()?;
    DeviceConfig::from_bytes(&buf)
}

pub fn save_device(flash: &mut FlashStorage, config: &DeviceConfig) -> Result<(), ()> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).map_err(|_| ())?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .map_err(|_| ())?
        .ok_or(())?;
    let mut part = nvs.as_embedded_storage(flash);
    let bytes = config.as_bytes();
    let mut aligned = [0u8; 64];
    aligned[..bytes.len()].copy_from_slice(bytes);
    part.write(DEVICE_CONFIG_OFFSET, &aligned).map_err(|_| ())
}

/// Outbound "auto-connect" config: the adapter dials a server and bridges SLCAN
/// over it (works from behind NAT). Stored as a single URL string of the form
///   `tcp://host:port/`  or  `tls://host:port/?pubkey=<hex>&token=<hex>`
/// (parsed by `autoconnect::parse_url`). Persisted to flash.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct AutoConnectConfig {
    magic: u32,
    enable: u8,
    _pad: u8,
    url_len: u16,
    url: [u8; URL_MAX],
}

impl AutoConnectConfig {
    pub const fn new() -> Self {
        Self {
            magic: 0,
            enable: 0,
            _pad: 0,
            url_len: 0,
            url: [0; URL_MAX],
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == AUTOCONN_CONFIG_MAGIC
    }

    pub fn enabled(&self) -> bool {
        self.enable != 0 && self.url_len > 0
    }

    /// Raw enable flag (for reflecting the checkbox state).
    pub fn enable_set(&self) -> bool {
        self.enable != 0
    }

    pub fn url_str(&self) -> &str {
        let n = (self.url_len as usize).min(URL_MAX);
        if n == 0 {
            ""
        } else {
            core::str::from_utf8(&self.url[..n]).unwrap_or("")
        }
    }

    pub fn set(&mut self, enable: bool, url: &str) {
        self.magic = AUTOCONN_CONFIG_MAGIC;
        self.enable = enable as u8;
        let n = url.len().min(URL_MAX);
        self.url_len = n as u16;
        self.url[..n].copy_from_slice(&url.as_bytes()[..n]);
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let config = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) };
        if config.is_valid() { Some(config) } else { None }
    }
}

pub fn load_autoconnect(flash: &mut FlashStorage) -> Option<AutoConnectConfig> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).ok()?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .ok()??;
    let mut part = nvs.as_embedded_storage(flash);
    let mut buf = [0u8; 384];
    part.read(AUTOCONN_CONFIG_OFFSET, &mut buf).ok()?;
    AutoConnectConfig::from_bytes(&buf)
}

pub fn save_autoconnect(flash: &mut FlashStorage, config: &AutoConnectConfig) -> Result<(), ()> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).map_err(|_| ())?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .map_err(|_| ())?
        .ok_or(())?;
    let mut part = nvs.as_embedded_storage(flash);
    let bytes = config.as_bytes();
    let mut aligned = [0u8; 384];
    aligned[..bytes.len()].copy_from_slice(bytes);
    part.write(AUTOCONN_CONFIG_OFFSET, &aligned).map_err(|_| ())
}

/// Web-interface login credentials, persisted to flash. The password is stored
/// only as a salted SHA-256 hash (per-credential random salt); the username is
/// kept in clear so it can be shown on the page. When no credentials are set the
/// web interface is open; once set, HTTP Basic Auth is required (holding GPIO0
/// bypasses it — see `http.rs`).
///
/// The hash is a single salted SHA-256 rather than an iterated/memory-hard KDF
/// on purpose: this device's flash already holds the WiFi password in clear (it
/// must, to reconnect) and physical access bypasses auth via the GPIO0 BOOT
/// button, so offline-crack resistance of *this* hash adds little, while a heavy
/// KDF would burn CPU on every `/api/status` poll on a real-time CAN bridge and
/// invite a wrong-password DoS. The salt defeats precomputed/cross-device hash
/// correlation; `verify` compares in constant time.
#[repr(C, align(4))]
#[derive(Clone, Copy)]
pub struct AuthConfig {
    magic: u32,
    user: [u8; 32],
    user_len: u8,
    has_pass: u8,
    _pad: [u8; 2],
    salt: [u8; 16],
    pass_hash: [u8; 32],
}

impl AuthConfig {
    pub const fn new() -> Self {
        Self {
            magic: 0,
            user: [0; 32],
            user_len: 0,
            has_pass: 0,
            _pad: [0; 2],
            salt: [0; 16],
            pass_hash: [0; 32],
        }
    }

    fn is_valid(&self) -> bool {
        self.magic == AUTH_CONFIG_MAGIC
    }

    /// True when both a username and a password are configured, i.e. auth is
    /// enforced.
    pub fn is_set(&self) -> bool {
        self.is_valid() && self.user_len > 0 && self.has_pass != 0
    }

    pub fn user_str(&self) -> &str {
        if self.user_len == 0 {
            ""
        } else {
            unsafe { core::str::from_utf8_unchecked(&self.user[..self.user_len as usize]) }
        }
    }

    /// Set credentials. An empty username or password clears them (disables
    /// the login requirement). A fresh random salt is drawn each time.
    pub fn set(&mut self, user: &str, password: &str) {
        self.magic = AUTH_CONFIG_MAGIC;
        if user.is_empty() || password.is_empty() {
            self.user_len = 0;
            self.has_pass = 0;
            self.user = [0; 32];
            self.salt = [0; 16];
            self.pass_hash = [0; 32];
            return;
        }
        self.user_len = user.len().min(32) as u8;
        self.user[..self.user_len as usize]
            .copy_from_slice(&user.as_bytes()[..self.user_len as usize]);
        let rng = Rng::new();
        for chunk in self.salt.chunks_mut(4) {
            let bytes = rng.random().to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
        self.pass_hash = salted_hash(&self.salt, password.as_bytes());
        self.has_pass = 1;
    }

    /// Check a username/password pair against the stored credentials. The
    /// password hash is compared in constant time; the username is not secret
    /// (it is shown on the page), so its comparison need not be.
    pub fn verify(&self, user: &str, password: &str) -> bool {
        if !self.is_set() {
            return true;
        }
        if user.as_bytes() != &self.user[..self.user_len as usize] {
            return false;
        }
        let computed = salted_hash(&self.salt, password.as_bytes());
        ct_eq(&computed, &self.pass_hash)
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            core::slice::from_raw_parts(self as *const Self as *const u8, core::mem::size_of::<Self>())
        }
    }

    fn from_bytes(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < core::mem::size_of::<Self>() {
            return None;
        }
        let config = unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) };
        if config.is_valid() { Some(config) } else { None }
    }
}

fn salted_hash(salt: &[u8], password: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(salt);
    h.update(password);
    let out = h.finalize();
    let mut buf = [0u8; 32];
    buf.copy_from_slice(&out);
    buf
}

/// Constant-time equality for two 32-byte hashes (no early-out on first
/// differing byte, so comparison time doesn't leak how much matched).
fn ct_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

pub fn load_auth(flash: &mut FlashStorage) -> Option<AuthConfig> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).ok()?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .ok()??;
    let mut part = nvs.as_embedded_storage(flash);
    let mut buf = [0u8; 128];
    part.read(AUTH_CONFIG_OFFSET, &mut buf).ok()?;
    AuthConfig::from_bytes(&buf)
}

pub fn save_auth(flash: &mut FlashStorage, config: &AuthConfig) -> Result<(), ()> {
    let mut pt_mem = [0u8; partitions::PARTITION_TABLE_MAX_LEN];
    let pt = partitions::read_partition_table(flash, &mut pt_mem).map_err(|_| ())?;
    let nvs = pt
        .find_partition(partitions::PartitionType::Data(
            partitions::DataPartitionSubType::Nvs,
        ))
        .map_err(|_| ())?
        .ok_or(())?;
    let mut part = nvs.as_embedded_storage(flash);
    let bytes = config.as_bytes();
    let mut aligned = [0u8; 128];
    aligned[..bytes.len()].copy_from_slice(bytes);
    part.write(AUTH_CONFIG_OFFSET, &aligned).map_err(|_| ())
}
