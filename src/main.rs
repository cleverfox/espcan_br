//! espcan_br — SLCAN CAN bridge over UART and WiFi/TCP, for the WeAct CAN485 ESP32.
//!
//! Transports: UART0 (the USB serial port) and TCP port 2000 on both the AP and STA
//! WiFi stacks. Each is an independent SLCAN "port" (own `O`/`C` open state); the
//! shared TWAI controller is on-bus while any port is open, and received bus frames
//! are broadcast to every open port. See PLAN.md. Previous blocking firmware is in
//! `legacy-beta0/`.
//!
//! Board pins: CAN RX=GPIO26 / TX=GPIO27, UART0 RX=GPIO3 / TX=GPIO1 (USB bridge),
//! WS2812 activity LED=GPIO4. The console logger is intentionally disabled so it
//! cannot corrupt the SLCAN stream on UART0.

#![no_std]
#![no_main]

extern crate alloc;

mod autoconnect;
mod can;
mod config;
mod http;
mod led;
mod mdns;
mod slcan;
mod transport;
mod uart_poll;

use core::cell::Cell;
use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};

use embassy_executor::Spawner;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpListenEndpoint, Ipv4Cidr, Runner, StackResources, StaticConfigV4};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex as BlockingMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{Duration, Timer};
use esp_alloc as _;
use esp_backtrace as _;
use esp_hal::clock::CpuClock;
use esp_hal::gpio::{Input, InputConfig, Pull};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::ram;
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::rng::Rng;
use esp_hal::time::Rate;
use esp_hal::timer::timg::TimerGroup;
use esp_hal::uart::{self, Uart, UartRx, UartTx};
use esp_hal::Blocking;
use esp_hal_dhcp_server::simple_leaser::SimpleDhcpLeaser;
use esp_hal_dhcp_server::structs::DhcpServerConfig;
use esp_radio::wifi::{
    ap::AccessPointConfig, sta::StationConfig, ModeConfig, WifiController, WifiDevice, WifiEvent,
};
use esp_storage::FlashStorage;

use config::{AuthConfig, AutoConnectConfig, DeviceConfig, WifiConfig};
use led::ActivityLed;

esp_bootloader_esp_idf::esp_app_desc!();

const AP_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);
const DHCP_POOL_START: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 10);
const DHCP_POOL_END: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 100);

/// TCP port that speaks SLCAN.
const SLCAN_TCP_PORT: u16 = 2000;

/// LED activity window: 10 buckets x 100 ms = 1 s.
const LED_TICK_MS: u64 = 100;
const LED_BUCKETS: usize = 10;
/// Blue status-LED level (~1/4 of full 255).
const STATUS_BLUE: u8 = 10;

// Shared state (read by the HTTP server and tasks).
pub(crate) static WIFI_CONFIG: Mutex<CriticalSectionRawMutex, WifiConfig> =
    Mutex::new(WifiConfig::new());
pub(crate) static DEVICE_CONFIG: Mutex<CriticalSectionRawMutex, DeviceConfig> =
    Mutex::new(DeviceConfig::new());
pub(crate) static AUTOCONNECT_CONFIG: Mutex<CriticalSectionRawMutex, AutoConnectConfig> =
    Mutex::new(AutoConnectConfig::new());
/// Web-interface login credentials (HTTP Basic Auth). Empty = open.
pub(crate) static AUTH_CONFIG: Mutex<CriticalSectionRawMutex, AuthConfig> =
    Mutex::new(AuthConfig::new());
/// `true` while the GPIO0 (BOOT) button is held — the physical owner override
/// that bypasses web-interface authentication (so a forgotten password can be
/// reset). Sampled by `gpio0_monitor`, never as a boot strap.
pub(crate) static GPIO0_HELD: AtomicBool = AtomicBool::new(false);
pub(crate) static STA_IP: Mutex<CriticalSectionRawMutex, Option<Ipv4Addr>> = Mutex::new(None);
pub(crate) static WIFI_CONNECTED: AtomicBool = AtomicBool::new(false);
pub(crate) static CONFIG_MODE: AtomicBool = AtomicBool::new(false);
/// Number of currently-connected TCP SLCAN clients (across AP + STA).
pub(crate) static TCP_CLIENTS: AtomicU32 = AtomicU32::new(0);
/// Auto-connect status code (see `autoconnect::state`), shown on the web page.
pub(crate) static AUTOCONNECT_STATE: AtomicU8 = AtomicU8::new(0);
/// Public key (uncompressed P-256, 65 bytes) of the last TLS server we connected
/// to — captured during the handshake and shown on the web page for pinning.
pub(crate) static SERVER_PUBKEY: BlockingMutex<CriticalSectionRawMutex, Cell<Option<[u8; 65]>>> =
    BlockingMutex::new(Cell::new(None));

macro_rules! mk_static {
    ($t:ty, $val:expr) => {{
        static STATIC_CELL: static_cell::StaticCell<$t> = static_cell::StaticCell::new();
        #[deny(unused_attributes)]
        let x = STATIC_CELL.uninit().write(($val));
        x
    }};
}

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let hw = esp_hal::Config::default().with_cpu_clock(CpuClock::max());
    let peripherals = esp_hal::init(hw);

    // Heap in bootloader-reclaimed DRAM2 (~98 KiB), separate from the stack.
    esp_alloc::heap_allocator!(#[ram(reclaimed)] size: 96 * 1024);

    // ── Load WiFi + device config; decide mode ─────────────────────────
    let mut flash = FlashStorage::new(peripherals.FLASH);
    let saved = config::load(&mut flash);
    let dev = config::load_device(&mut flash).unwrap_or_else(DeviceConfig::defaults);
    let auto = config::load_autoconnect(&mut flash).unwrap_or_else(AutoConnectConfig::new);
    let authcfg = config::load_auth(&mut flash).unwrap_or_else(AuthConfig::new);
    drop(flash);
    let config_mode = !saved.map(|c| c.is_valid()).unwrap_or(false);
    CONFIG_MODE.store(config_mode, Ordering::Relaxed);
    if let Some(c) = saved {
        *WIFI_CONFIG.lock().await = c;
    }
    *DEVICE_CONFIG.lock().await = dev; // DeviceConfig is Copy; `dev` stays usable
    *AUTOCONNECT_CONFIG.lock().await = auto;
    *AUTH_CONFIG.lock().await = authcfg;

    // ── Start the RTOS / embassy runtime ───────────────────────────────
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    // ── UART0 SLCAN link (USB bridge: GPIO3 RX / GPIO1 TX) ─────────────
    // Blocking driver (polled): UART0 is the system console, and the async UART
    // driver does not deliver RX on it under esp-rtos. See uart_poll.rs.
    let uart = Uart::new(
        peripherals.UART0,
        uart::Config::default().with_baudrate(dev.baud()),
    )
    .unwrap()
    .with_rx(peripherals.GPIO3)
    .with_tx(peripherals.GPIO1);
    let (uart_rx, uart_tx) = uart.split();

    // ── WS2812 activity LED on GPIO4 via RMT ───────────────────────────
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let led_channel = rmt
        .channel0
        .configure_tx(&TxChannelConfig::default().with_clk_divider(1))
        .unwrap()
        .with_pin(peripherals.GPIO4);
    let mut led = ActivityLed::new(led_channel);
    led.set_rgb(0, 0, 0);

    // ── GPIO0 (BOOT button) — physical auth-bypass override ────────────
    // Sampled only at runtime (never as a boot strap). Active-low: the button
    // pulls the pin to GND, so "held" == low.
    let gpio0 = Input::new(
        peripherals.GPIO0,
        InputConfig::default().with_pull(Pull::Up),
    );

    // ── WiFi ───────────────────────────────────────────────────────────
    let (mut controller, interfaces) =
        esp_radio::wifi::new(peripherals.WIFI, Default::default()).unwrap();
    let ap_device = interfaces.access_point;
    let sta_device = interfaces.station;

    let ap_config = embassy_net::Config::ipv4_static(StaticConfigV4 {
        address: Ipv4Cidr::new(AP_IP, 24),
        gateway: Some(AP_IP),
        dns_servers: Default::default(),
    });
    let sta_config = embassy_net::Config::dhcpv4(Default::default());

    let rng = Rng::new();
    let net_seed = (rng.random() as u64) << 32 | rng.random() as u64;

    let (ap_stack, ap_runner) = embassy_net::new(
        ap_device,
        ap_config,
        mk_static!(StackResources<4>, StackResources::<4>::new()),
        net_seed,
    );
    let (sta_stack, sta_runner) = embassy_net::new(
        sta_device,
        sta_config,
        mk_static!(StackResources<6>, StackResources::<6>::new()),
        net_seed,
    );

    if config_mode {
        let ap = AccessPointConfig::default().with_ssid(dev.name_str().into());
        controller.set_config(&ModeConfig::AccessPoint(ap)).unwrap();
    } else {
        let cfg = WIFI_CONFIG.lock().await;
        let sta = StationConfig::default()
            .with_ssid(cfg.ssid_str().into())
            .with_password(cfg.password_str().into());
        drop(cfg);
        controller.set_config(&ModeConfig::Station(sta)).unwrap();
    }
    controller.start_async().await.unwrap();

    // ── Tasks ──────────────────────────────────────────────────────────
    spawner.spawn(gpio0_monitor(gpio0)).ok();
    spawner.spawn(can::can_rx_task()).ok();
    spawner.spawn(uart_port(uart_rx, uart_tx)).ok();
    spawner.spawn(tcp_port(ap_stack)).ok();
    spawner.spawn(tcp_port(sta_stack)).ok();
    spawner.spawn(net_task(ap_runner)).ok();
    spawner.spawn(net_task(sta_runner)).ok();
    spawner.spawn(sta_ip_monitor(sta_stack)).ok();
    spawner.spawn(dhcp_server(ap_stack)).ok();
    spawner.spawn(http::http_server(ap_stack, "AP")).ok();
    spawner.spawn(http::http_server(sta_stack, "STA")).ok();
    spawner.spawn(mdns::responder_task(sta_stack)).ok();
    spawner.spawn(autoconnect::autoconnect_task(sta_stack)).ok();
    if !config_mode {
        spawner.spawn(connection_task(controller)).ok();
    }

    // ── Activity LED tick (sliding 1 s window over RX/TX counters) ──────
    let mut rx_buckets = [0u32; LED_BUCKETS];
    let mut tx_buckets = [0u32; LED_BUCKETS];
    let mut idx = 0usize;
    let mut last_rx = 0u32;
    let mut last_tx = 0u32;
    loop {
        Timer::after(Duration::from_millis(LED_TICK_MS)).await;
        let rx = can::RX_COUNT.load(Ordering::Relaxed);
        let tx = can::TX_COUNT.load(Ordering::Relaxed);
        rx_buckets[idx] = rx.wrapping_sub(last_rx);
        tx_buckets[idx] = tx.wrapping_sub(last_tx);
        last_rx = rx;
        last_tx = tx;
        let g = led::level_from_count(rx_buckets.iter().sum());
        let r = led::level_from_count(tx_buckets.iter().sum());

        // Blue = status, ~1 s cycle (idx 0..9). TCP client connected: mostly on
        // (100 ms off / 900 ms on). Else WiFi ready: brief blink (900 ms off /
        // 100 ms on). Else (connecting): off.
        let tcp_connected = TCP_CLIENTS.load(Ordering::Relaxed) > 0;
        let wifi_ready = CONFIG_MODE.load(Ordering::Relaxed) || WIFI_CONNECTED.load(Ordering::Relaxed);
        let b = if tcp_connected {
            if idx == 0 { 0 } else { STATUS_BLUE }
        } else if wifi_ready {
            if idx == 0 { STATUS_BLUE } else { 0 }
        } else {
            0
        };

        led.set_rgb(r, g, b);
        idx = (idx + 1) % LED_BUCKETS;
    }
}

#[embassy_executor::task]
async fn uart_port(rx: UartRx<'static, Blocking>, tx: UartTx<'static, Blocking>) {
    let mut sub = can::rx_subscriber();
    let mut conn = uart_poll::PolledUart::new(rx, tx);
    transport::run_port(&mut conn, &mut sub, transport::Iface::Serial).await;
}

#[embassy_executor::task(pool_size = 2)]
async fn tcp_port(stack: embassy_net::Stack<'static>) {
    let mut rx_buffer = [0u8; 1024];
    let mut tx_buffer = [0u8; 1024];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buffer, &mut tx_buffer);
        socket.set_timeout(Some(Duration::from_secs(300)));
        if socket
            .accept(IpListenEndpoint {
                addr: None,
                port: SLCAN_TCP_PORT,
            })
            .await
            .is_err()
        {
            Timer::after(Duration::from_millis(500)).await;
            continue;
        }
        let mut sub = can::rx_subscriber();
        TCP_CLIENTS.fetch_add(1, Ordering::Relaxed);
        transport::run_port(&mut socket, &mut sub, transport::Iface::Tcp).await;
        TCP_CLIENTS.fetch_sub(1, Ordering::Relaxed);
        socket.close();
        Timer::after(Duration::from_millis(50)).await;
        socket.abort();
    }
}

/// Polls the GPIO0 (BOOT) button and publishes its held state. Holding it
/// bypasses web-interface auth so a forgotten password can be reset.
#[embassy_executor::task]
async fn gpio0_monitor(pin: Input<'static>) {
    loop {
        GPIO0_HELD.store(pin.is_low(), Ordering::Relaxed);
        Timer::after(Duration::from_millis(100)).await;
    }
}

#[embassy_executor::task]
async fn connection_task(mut controller: WifiController<'static>) {
    loop {
        if matches!(controller.is_started(), Ok(true)) {
            match controller.connect_async().await {
                Ok(_) => {
                    WIFI_CONNECTED.store(true, Ordering::Relaxed);
                    controller
                        .wait_for_event(WifiEvent::StationDisconnected)
                        .await;
                    WIFI_CONNECTED.store(false, Ordering::Relaxed);
                }
                Err(_) => {
                    Timer::after(Duration::from_millis(5000)).await;
                }
            }
        } else {
            return;
        }
    }
}

#[embassy_executor::task(pool_size = 2)]
async fn net_task(mut runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await
}

#[embassy_executor::task]
async fn sta_ip_monitor(stack: embassy_net::Stack<'static>) {
    loop {
        let current = stack.config_v4().map(|c| c.address.address());
        {
            let mut ip = STA_IP.lock().await;
            *ip = current;
        }
        Timer::after(Duration::from_millis(1000)).await;
    }
}

#[embassy_executor::task]
async fn dhcp_server(stack: embassy_net::Stack<'static>) {
    let config = DhcpServerConfig {
        ip: AP_IP,
        lease_time: Duration::from_secs(3600),
        gateways: &[AP_IP],
        subnet: None,
        dns: &[AP_IP],
        use_captive_portal: false,
    };
    let mut leaser = SimpleDhcpLeaser {
        start: DHCP_POOL_START,
        end: DHCP_POOL_END,
        leases: Default::default(),
    };
    if let Err(_e) = esp_hal_dhcp_server::run_dhcp_server(stack, config, &mut leaser).await {}
}
