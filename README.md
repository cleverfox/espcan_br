# espcan_br

An **SLCAN / LAWICEL (CANUSB)** CAN bridge for the
**WeAct CAN485 DevBoard V1 (classic ESP32)**, written in `no_std` async Rust on
[`esp-hal`](https://github.com/esp-rs/esp-hal) + `esp-radio` (WiFi) + `embassy`.

It exposes the CAN bus as an SLCAN adapter over **two transports at once**: the
**USB serial port (UART0)** and a **TCP server on port 2000** over WiFi. Each is an
independent SLCAN port — drive either with Linux `slcand`/`can-utils` (or the
included `tools/slcan_tool.py`). The on-the-wire ASCII protocol is unchanged from
the original STM32 `uart_can_br`, so existing hosts work as-is.

> The earlier simple, blocking, UART-only firmware (esp-hal `1.0.0-beta.0`) is
> preserved under `legacy-beta0/`. Architecture/roadmap: see `PLAN.md`.

## Hardware

WeAct CAN485 DevBoard V1, classic ESP32 (Xtensa, dual-core). Relevant pins:

| Function        | GPIO            | Notes                                  |
|-----------------|-----------------|----------------------------------------|
| CAN RX          | GPIO26          | to onboard CAN transceiver             |
| CAN TX          | GPIO27          | to onboard CAN transceiver             |
| Host UART (TX)  | GPIO1           | UART0 → onboard USB-UART bridge        |
| Host UART (RX)  | GPIO3           | UART0 → onboard USB-UART bridge        |
| Activity LED    | GPIO4           | onboard WS2812B (1 LED, GRB), via RMT  |

The classic ESP32 has **no native USB**, so the UART host link is UART0 routed
through the board's onboard USB-UART chip — i.e. the same `/dev/ttyUSB*` you flash
over. The console **logger is disabled** so it cannot corrupt the SLCAN byte stream
on UART0.

## WiFi & TCP

The firmware speaks SLCAN over WiFi/TCP **on port 2000**, in addition to UART0.

- **Modes.** With no saved credentials it boots into **config mode**: a WiFi AP
  `espcan-br` (192.168.4.1, DHCP) serving an HTTP setup page. Enter your SSID +
  password → it saves to flash and reboots into **STA mode** (joins your network).
- **HTTP config** is reachable on both the AP IP and, in STA mode, the device's
  station IP (shown on the page; a UART-speed setting is planned here).
- **Multi-port semantics.** UART and each TCP connection are independent SLCAN
  ports, each activated by its own `O`/`C` (a TCP disconnect implicitly closes its
  port). The shared CAN controller is on-bus while *any* port is open, and every
  received bus frame is **broadcast to all open ports**. `Sn` (bitrate) is only
  honoured while the channel is fully closed.

```sh
# Point slcand at the TCP port (SLCAN over TCP):
slcand -t <device-ip>:2000 can0   # or, on plain SLCAN-over-TCP hosts, nc <ip> 2000
# Or the included tool over TCP — see tools/slcan_tool.py (UART today; TCP host = same protocol)
```

> Note: `make monitor` opens UART0, which now carries the **SLCAN byte stream**
> (not logs). To debug WiFi bring-up, watch the LED / web page, or temporarily
> re-enable a logger on a spare UART.

### Auto-connect (outbound)

For use behind NAT, the adapter can **dial out** to a server and bridge SLCAN over
that connection instead of (or in addition to) waiting for inbound TCP. Set one URL
on the web page; it reconnects automatically and shows a live status.

URL format:

```
tcp://host:port/
tls://host:port/?pubkey=<130-hex>&token=<hex>
```

- The server is the SLCAN peer — e.g. `slcand -p <port> slcan0` on a public host
  (plaintext), or a TLS-terminating front (e.g. stunnel) for the TLS case. Server
  must offer TLS 1.3 / `AES_128_GCM_SHA256` with an **ECDSA P-256** key.
- **`pubkey`** (optional, `tls://` only): the server's uncompressed P-256 public
  key (`04`+X+Y = 130 hex). When set, the handshake's `CertificateVerify` signature
  must validate under this key, so the **server is authenticated by key pinning** —
  no CA needed; a mismatched/MITM server is rejected (status shows the failure).
  Without `pubkey`, TLS is **encrypt-only** (no server authentication).
- **`token`** (optional): sent as the auth command `H<token>\r` immediately after
  the link is up, to identify this adapter to the server.

To get a server's `pubkey`: connect once **without** it (`tls://host:port/`); the
web page shows **"Server key (last TLS connection)"** — copy that 130-hex value
into `?pubkey=` to enable pinning.

> TLS pinning needs a small fork of `embedded-tls` 0.18 (upstream doesn't export
> the cert types its verifier trait names). It's pinned by `git`/`rev` in
> `Cargo.toml` ([cleverfox/embedded-tls](https://github.com/cleverfox/embedded-tls)).

> **Boot note (from the board docs):** holding the GPIO0 KEY while powering on
> keeps the chip in download/boot mode; power-cycle again to run. Remove any TF
> card before flashing or flashing fails.

## Toolchain setup (one-time)

The classic ESP32 is Xtensa, so it needs the `esp` Rust toolchain fork:

```sh
cargo install espup espflash
espup install          # installs the `esp` toolchain + Xtensa GCC + clang
# espup writes ~/export-esp.sh (the Makefile sources it for the linker)
```

## Build / flash

```sh
make            # build release
make flash      # build, flash over USB, open serial monitor (cargo run)
make monitor    # open serial monitor only
make size       # show firmware size
```

The `Makefile` ensures the esp toolchain's `cargo`/`rustc` are used (so a
Homebrew `cargo` on `PATH` can't shadow them) and that the bundled GCC linker is
available. Equivalent manual invocation:

```sh
source ~/export-esp.sh
export PATH="$HOME/.rustup/toolchains/esp/bin:$PATH"
cargo run --release
```

**Dependencies.** The WiFi build pins the esp-hal stack (`esp-hal`, `esp-radio`,
`esp-rtos`, …) to a fork of esp-hal `main`
([cleverfox/esp-hal@espcan-br](https://github.com/cleverfox/esp-hal/tree/espcan-br))
carrying two firmware-required patches (an esp-radio WiFi-timer deferred-arm fix
and an Xtensa fn-ptr cast); see the `git`/`rev` entries in `Cargo.toml`. The image
embeds an ESP-IDF app descriptor (`esp_app_desc!`), so `espflash` flashes it with
no extra flags.

## Host usage (Linux SocketCAN)

```sh
# -s6 = 500 kbit (see table), -S = host serial baud (must match firmware), 115200
sudo slcand -o -c -s6 -S 115200 /dev/ttyUSB0 can0
sudo ip link set up can0

candump can0
cansend can0 123#DEADBEEF
```

You can also talk to it directly in a terminal: open the port at 115200 8N1 and
type SLCAN commands (each terminated by CR).

## Test tool (`tools/slcan_tool.py`)

A standalone Python tool (pyserial only — no SocketCAN/slcand needed) that opens
the adapter's serial port, drives it to transmit generated frames at a chosen
rate, and live-decodes inbound frames. Handy for bench/scope testing.

```sh
pip install pyserial        # once (or use your venv)

# 500 kbit, send std id 0x123 = DEADBEEF at 50 frames/s, show inbound frames:
python tools/slcan_tool.py -p /dev/cu.usbmodemXXXX -b 6 -i 123 -d DEADBEEF -r 50

# incrementing 8-byte counter on an extended id at 200 fps:
python tools/slcan_tool.py -p /dev/cu.usbmodemXXXX -i 18FF50E5 --ext \
    --pattern inc --len 8 -r 200

# receive only (just display bus frames):
python tools/slcan_tool.py -p /dev/cu.usbmodemXXXX --listen
```

Key options: `-b` CAN bitrate code (0-8), `-i/--id/--address` (hex), `--ext`,
`--rtr`, `--pattern fixed|inc|random`, `-d/--data` (hex), `--len`, `-r/--rate`
(fps, 0 = max), `-c/--count`, `-v` (show acks/naks + TX echo). `python
tools/slcan_tool.py -h` for all. It sends the same `C`/`S<d>`/`O` startup as
slcand, so the adapter goes bus-on automatically.

### Over the can_router TLS relay (`tools/slcan_router_tool.py`)

Same tool, but it connects to a [can_router](../can_router/DESIGN.md) relay over
**TLS** and authenticates with a token URL (stdlib `ssl` only). It **pins the
server public key** (from the URL; mismatch aborts) and sends `H<token>\r`:

```sh
python tools/slcan_router_tool.py \
  'tls://192.168.1.21:9996/?pubkey=04..&token=84..' -b 6 -i 123 -d DEADBEEF -r 50
# receive only:
python tools/slcan_router_tool.py 'tls://host:9996/?pubkey=04..&token=84..' --listen
```

Pinning is mandatory unless `--insecure` is passed. Once the relay pairs it with
the bridge, it behaves exactly like `slcan_tool.py` (probes `V`, then `C/S/O`,
then generates/displays frames).

### TLS proxy for slcand (`tools/slcan_tls_proxy.py`)

slcand has no TLS/pinning/token support, so this proxy bridges it to a can_router
relay: it listens on a local TCP port and, per connection, opens a pinned TLS
session to the relay, injects `H<token>\r`, then relays bytes verbatim.

```sh
python tools/slcan_tls_proxy.py \
  'tls://192.168.1.21:9996/?pubkey=04..&token=84..' -l 127.0.0.1:2000 &
slcand -o -s6 -t 127.0.0.1:2000 slcan0 && ifconfig slcan0 up   # FreeBSD
```

Pinning mandatory unless `--insecure`. It never parses SLCAN for the relay —
slcand and the bridge speak end-to-end — but `-v` additionally **decodes and
prints every relayed frame/line** (`TX` = host→bus, `RX` = bus→host; the auth
token is redacted).

## SLCAN protocol

Commands are ASCII, terminated by `\r` (CR). Replies are `\r` for OK and
`0x07` (BEL) for error.

| Cmd                    | Meaning                                   | Reply        |
|------------------------|-------------------------------------------|--------------|
| `Sn`                   | Set CAN bitrate (channel must be closed)  | CR / BEL     |
| `O`                    | Open channel (normal mode)                | CR           |
| `L`                    | Open channel (listen-only)                | CR           |
| `C`                    | Close channel                             | CR           |
| `tIIILDD…`             | TX standard data frame (3-hex id)         | `z\r` / BEL  |
| `TIIIIIIIILDD…`        | TX extended data frame (8-hex id)         | `Z\r` / BEL  |
| `rIIIL`                | TX standard remote frame                  | `z\r` / BEL  |
| `RIIIIIIIIL`           | TX extended remote frame                  | `Z\r` / BEL  |
| `V`                    | Hardware/firmware version                 | `V1013\r`    |
| `N`                    | Serial number                             | `N1234\r`    |
| `F`                    | Status flags (real, see below)            | `Fxx\r`      |
| `Z0` / `Z1`            | RX timestamps off / on                    | CR           |
| `M` / `m`              | Acceptance code/mask (accepted, no-op)    | CR           |
| `A`                    | Auto-retransmit (accepted, no-op)         | CR           |
| `sXXYY`                | Custom BTR registers                      | BEL (unsupported) |

Transmit success returns the Lawicel confirmation `z\r` (11-bit) / `Z\r` (29-bit);
`F` returns a real 8-bit status byte from the TWAI error state — bit 2 = error
warning, bit 5 = error passive, bit 7 = bus-off (0 when the channel is closed).

Received frames are pushed to the host in the same `t/T/r/R` format, with a
4-hex-digit millisecond timestamp appended (before the CR) when `Z1` is active.

### Bitrate table (`Sn`)

| `Sn` | Bitrate  | Supported | Timing (80 MHz TWAI clock)                    |
|------|----------|-----------|-----------------------------------------------|
| S0   | 10 kbit  | -         | prescaler exceeds classic-ESP32 hardware range |
| S1   | 20 kbit  | -         | prescaler exceeds classic-ESP32 hardware range |
| S2   | 50 kbit  | +         | brp 80, tseg 15/4, sjw 3                       |
| S3   | 100 kbit | +         | brp 40, tseg 15/4, sjw 3                       |
| S4   | 125 kbit | +         | esp-hal `B125K`                               |
| S5   | 250 kbit | +         | esp-hal `B250K`                               |
| S6   | 500 kbit | +         | esp-hal `B500K`                               |
| S7   | 800 kbit | +         | brp 4, tseg 16/8, sjw 3                        |
| S8   | 1 Mbit   | +         | esp-hal `B1000K`                              |

`S0`/`S1` return BEL — 10/20 kbit need a baud prescaler larger than the classic
ESP32's TWAI hardware supports. If unset, the bridge defaults to **S6 (500 kbit)**.

## Activity LED

The onboard WS2812B (GPIO4) shows live CAN activity:

- **Green** brightness ∝ inbound frames/s (CAN bus → host).
- **Red** brightness ∝ outbound frames/s (host → CAN bus).
- Both directions at once → **yellow**.
- Idle → off; brightness rises with the per-second frame rate and decays smoothly
  (~1 s) as traffic stops.

It's driven directly over esp-hal's RMT peripheral (no extra crate). The counting
uses a 10×100 ms sliding window refreshed at 10 Hz. Tunables live in `src/led.rs`
(`FRAMES_FOR_FULL`, `MAX_LEVEL`, `MIN_LEVEL`) and `src/main.rs` (`LED_TICK_MS`,
`WINDOW_BUCKETS`). The pure `level_from_count()` mapping is hardware-agnostic and
unit-testable; the WS2812 bit timing (`T0H/T0L/T1H/T1L` in `led.rs`) is
scope-verifiable on GPIO4 if colours look off.

## Design notes

- Single blocking superloop (no RTOS): drain UART → parse commands; drain TWAI RX
  → emit frames. No interrupts/queues needed at these rates.
- `src/slcan.rs` is pure, hardware-agnostic protocol code (parse/format over a
  neutral `CanFrame`); `src/main.rs` glues it to esp-hal's TWAI + UART0.
- The acceptance filter is set to accept-all; filtering is left to the host.
- TX uses a bounded busy-wait so a dead/unacked bus can't hang the bridge.
- The channel is re-opened (peripherals re-`steal()`ed) on each `O`, so the
  bitrate can change between `C`/`O` cycles.

## Limitations / TODO

- **CAN FD is not possible on this board.** The classic ESP32's TWAI controller is
  Classical-CAN (2.0) only — per Espressif it interprets FD frames as bus errors.
  FD would require an external SPI controller (MCP2518FD/MCP251863) or FD-capable
  silicon (ESP32-P4/C5). `sl_proto.md` Appendix A (the FD extension) is therefore
  out of scope for this firmware.
- `S0`/`S1` (10/20 kbit) unsupported (see above).
- Bus-error / overrun alerts are not yet surfaced to the host (`F` returns `00`).
- Pinned to `esp-hal = 1.0.0-beta.0` (the version this code was verified against).
  Bumping esp-hal may require touching the UART/TWAI calls and `now_ms()`.
