# espcan_br

An **SLCAN / LAWICEL (CANUSB)** CAN ↔ serial bridge for the
**WeAct CAN485 DevBoard V1 (classic ESP32)**, written in `no_std` Rust on
[`esp-hal`](https://github.com/esp-rs/esp-hal).

It turns the board into a standard serial-line CAN adapter: plug it into a host
over USB and drive it with Linux `slcand`/`can-utils` (or any SLCAN-aware tool).
This is a Rust port of the older STM32 `uart_can_br` firmware, keeping the same
on-the-wire ASCII protocol so existing hosts work unchanged.

## Hardware

WeAct CAN485 DevBoard V1, classic ESP32 (Xtensa, dual-core). Relevant pins:

| Function        | GPIO            | Notes                                  |
|-----------------|-----------------|----------------------------------------|
| CAN RX          | GPIO26          | to onboard CAN transceiver             |
| CAN TX          | GPIO27          | to onboard CAN transceiver             |
| Host UART (TX)  | GPIO1           | UART0 → onboard USB-UART bridge        |
| Host UART (RX)  | GPIO3           | UART0 → onboard USB-UART bridge        |
| Activity LED    | GPIO4           | onboard WS2812B (1 LED, GRB), via RMT  |

The classic ESP32 has **no native USB**, so the host link is UART0 routed through
the board's onboard USB-UART chip — i.e. the same `/dev/ttyUSB*` you flash over.

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

Flashing uses `espflash flash --monitor --ignore-app-descriptor` (see
`.cargo/config.toml`); the flag is only needed because esp-hal `1.0.0-beta.0`
images don't embed an ESP-IDF app descriptor.

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

## SLCAN protocol

Commands are ASCII, terminated by `\r` (CR). Replies are `\r` for OK and
`0x07` (BEL) for error.

| Cmd                    | Meaning                                   | Reply        |
|------------------------|-------------------------------------------|--------------|
| `Sn`                   | Set CAN bitrate (channel must be closed)  | CR / BEL     |
| `O`                    | Open channel (normal mode)                | CR           |
| `L`                    | Open channel (listen-only)                | CR           |
| `C`                    | Close channel                             | CR           |
| `tIIILDD…`             | TX standard data frame (3-hex id)         | CR / BEL     |
| `TIIIIIIIILDD…`        | TX extended data frame (8-hex id)         | CR / BEL     |
| `rIIIL`                | TX standard remote frame                  | CR / BEL     |
| `RIIIIIIIIL`           | TX extended remote frame                  | CR / BEL     |
| `V`                    | Hardware/firmware version                 | `V1013\r`    |
| `N`                    | Serial number                             | `N1234\r`    |
| `F`                    | Status flags                              | `F00\r`      |
| `Z0` / `Z1`            | RX timestamps off / on                    | CR           |
| `M` / `m`              | Acceptance code/mask (accepted, no-op)    | CR           |
| `sXXYY`                | Custom BTR registers                      | BEL (unsupported) |

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
