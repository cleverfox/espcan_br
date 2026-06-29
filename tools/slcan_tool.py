#!/usr/bin/env python3
"""SLCAN test tool for the espcan_br ESP32 bridge.

Opens the adapter's serial (UART) port, drives it to transmit generated CAN
frames at a configurable rate, and concurrently decodes and displays inbound
frames the adapter reports from the bus.

Speaks the same SLCAN/LAWICEL subset as FreeBSD slcand and the espcan_br firmware
(see sl_proto.md): startup `C` / `S<d>` / `O`, then `t/T/r/R` frame messages,
each terminated by CR.

Examples
--------
  # 500 kbit bus, send std id 0x123 with DEADBEEF, 50 frames/s, show RX:
  python slcan_tool.py -p /dev/cu.usbmodemXXXX -b 6 -i 123 -d DEADBEEF -r 50

  # incrementing 8-byte counter payload at 200 fps on extended id:
  python slcan_tool.py -p /dev/cu.usbmodemXXXX -b 6 -i 18FF50E5 --ext \
      --pattern inc --len 8 -r 200

  # receive only (just display inbound frames):
  python slcan_tool.py -p /dev/cu.usbmodemXXXX -b 6 --listen
"""

import argparse
import os
import random
import sys
import threading
import time

try:
    import serial  # pyserial
except ImportError:
    sys.exit("pyserial not found — `pip install pyserial` (or activate your venv)")

CR = b"\r"
BEL = 0x07

# SLCAN bitrate code -> human label (matches sl_proto.md §3 / firmware table).
BITRATES = {
    0: "10k", 1: "20k", 2: "50k", 3: "100k", 4: "125k",
    5: "250k", 6: "500k", 7: "800k", 8: "1M",
}


# --------------------------------------------------------------------------- #
# SLCAN frame encode / decode
# --------------------------------------------------------------------------- #
def encode_frame(can_id, data, ext=False, rtr=False):
    """Build an SLCAN transmit line (without the trailing CR), uppercase hex."""
    if ext:
        type_char = "R" if rtr else "T"
        id_str = f"{can_id & 0x1FFFFFFF:08X}"
    else:
        type_char = "r" if rtr else "t"
        id_str = f"{can_id & 0x7FF:03X}"
    dlc = len(data)
    line = f"{type_char}{id_str}{dlc:X}"
    if not rtr:
        line += "".join(f"{b:02X}" for b in data)
    return line


def decode_frame(line):
    """Decode an adapter->host SLCAN line into a dict, or None if not a frame.

    Tolerates an optional trailing 4-hex-digit timestamp (firmware `Z1` mode).
    """
    if not line:
        return None
    t = line[0]
    if t not in "tTrR":
        return None
    ext = t in "TR"
    rtr = t in "rR"
    idlen = 8 if ext else 3
    try:
        pos = 1
        can_id = int(line[pos:pos + idlen], 16)
        pos += idlen
        dlc = int(line[pos], 16)
        pos += 1
        data = b""
        if not rtr:
            hexbytes = line[pos:pos + dlc * 2]
            if len(hexbytes) < dlc * 2:
                return None
            data = bytes.fromhex(hexbytes)
            pos += dlc * 2
        # Anything left (4 hex) is an optional timestamp; ignore it.
        return {"id": can_id, "ext": ext, "rtr": rtr, "dlc": dlc, "data": data}
    except (ValueError, IndexError):
        return None


def fmt_frame(f):
    """candump-ish one-line rendering of a decoded frame."""
    width = 8 if f["ext"] else 3
    ids = f"{f['id']:0{width}X}"
    if f["rtr"]:
        body = f"remote len {f['dlc']}"
    else:
        body = " ".join(f"{b:02X}" for b in f["data"])
    kind = "EFF" if f["ext"] else "SFF"
    return f"{ids:<8} [{f['dlc']}] {kind}  {body}"


# --------------------------------------------------------------------------- #
# Payload generation
# --------------------------------------------------------------------------- #
def make_payload_gen(args):
    """Return a zero-arg callable that yields the next payload (bytes)."""
    if args.pattern == "fixed":
        try:
            fixed = bytes.fromhex(args.data)
        except ValueError:
            sys.exit(f"--data must be hex bytes, got {args.data!r}")
        length = args.len if args.len is not None else len(fixed)
        fixed = (fixed + bytes(8))[:length]  # pad/truncate to length
        return lambda: fixed

    length = args.len if args.len is not None else 8
    length = max(0, min(8, length))

    if args.pattern == "random":
        return lambda: bytes(random.randrange(256) for _ in range(length))

    # incrementing counter packed big-endian into `length` bytes
    state = {"n": 0}

    def inc():
        if length == 0:
            return b""
        val = state["n"] & ((1 << (8 * length)) - 1)
        state["n"] += 1
        return val.to_bytes(length, "big")

    return inc


# --------------------------------------------------------------------------- #
# Serial reader thread
# --------------------------------------------------------------------------- #
class Reader(threading.Thread):
    def __init__(self, ser, stats, verbose):
        super().__init__(daemon=True)
        self.ser = ser
        self.stats = stats
        self.verbose = verbose
        self._stop = threading.Event()

    def run(self):
        buf = bytearray()
        while not self._stop.is_set():
            try:
                chunk = self.ser.read(256)
            except serial.SerialException:
                break
            if not chunk:
                continue
            for byte in chunk:
                if byte == 0x0D:  # CR -> end of line
                    self._dispatch(bytes(buf))
                    buf.clear()
                elif byte == BEL:
                    self.stats["nak"] += 1
                    if self.verbose:
                        print("  <- BEL (nak)")
                elif byte != 0x0A:  # ignore LF
                    buf.append(byte)

    def _dispatch(self, raw):
        if not raw:
            self.stats["ack"] += 1  # bare CR = command ack
            return
        try:
            line = raw.decode("ascii", "replace")
        except Exception:
            return
        if line in ("z", "Z"):
            self.stats["ack"] += 1  # Lawicel transmit confirmation
            return
        f = decode_frame(line)
        if f is not None:
            self.stats["rx"] += 1
            print(f"RX  {fmt_frame(f)}")
        elif self.verbose:
            print(f"  <- {line!r}")

    def stop(self):
        self._stop.set()


# --------------------------------------------------------------------------- #
# Main
# --------------------------------------------------------------------------- #
def parse_args():
    p = argparse.ArgumentParser(
        description="SLCAN test tool: generate CAN frames over UART and display inbound frames.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument("-p", "--port", default=os.environ.get("SLCAN_PORT"),
                   help="serial device (e.g. /dev/cu.usbmodemXXXX); "
                        "defaults to $SLCAN_PORT")
    p.add_argument("-S", "--serial-baud", type=int, default=115200,
                   help="host serial baud (must match firmware SLCAN_UART_BAUD)")
    p.add_argument("-b", "--bitrate", type=int, default=6, choices=range(0, 9),
                   metavar="0-8", help="CAN bitrate code Sn (6=500k)")

    p.add_argument("-i", "--id", "--address", dest="id", default="123",
                   help="CAN identifier in hex")
    p.add_argument("--ext", action="store_true", help="extended (29-bit) id")
    p.add_argument("--rtr", action="store_true", help="send remote frames")

    p.add_argument("--pattern", choices=("fixed", "inc", "random"),
                   default="fixed", help="payload pattern")
    p.add_argument("-d", "--data", default="DEADBEEF",
                   help="payload hex for --pattern fixed")
    p.add_argument("--len", type=int, default=None,
                   help="payload length in bytes (0-8); default: len(data) or 8")

    p.add_argument("-r", "--rate", type=float, default=10.0,
                   help="frames per second to generate (0 = as fast as possible)")
    p.add_argument("-c", "--count", type=int, default=0,
                   help="number of frames to send (0 = unlimited)")

    p.add_argument("--listen", action="store_true",
                   help="receive only: open the channel but do not transmit")
    p.add_argument("--no-open", action="store_true",
                   help="skip the C/S/O startup sequence")
    p.add_argument("--boot-delay", type=float, default=1.5,
                   help="seconds to wait after opening the port (ESP32 may reset)")
    p.add_argument("--ready-timeout", type=float, default=15.0,
                   help="max seconds to wait for the firmware to answer 'V' after "
                        "the boot/reset before handshaking")
    p.add_argument("-v", "--verbose", action="store_true",
                   help="also show acks/naks and non-frame lines, and echo TX")
    args = p.parse_args()
    if not args.port:
        p.error("no serial port: pass -p/--port or set $SLCAN_PORT")
    try:
        args.id = int(args.id, 16)
    except ValueError:
        p.error(f"--id must be hex, got {args.id!r}")
    return args


def send_line(ser, text, verbose=False):
    ser.write(text.encode("ascii") + CR)
    if verbose:
        print(f"  -> {text!r}")


def wait_ready(ser, timeout, verbose=False):
    """Probe with 'V' until the firmware answers 'V1013'.

    Opening the USB port resets the ESP32 (DTR/RTS auto-reset); the WiFi firmware
    then takes a while to boot. Rather than guess a delay, poll the version command
    until the running app replies, so the handshake is never sent mid-boot. Must run
    before the reader thread starts (it reads the port directly). Returns True if the
    device answered.
    """
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        ser.reset_input_buffer()
        ser.write(b"V\r")
        if verbose:
            print("  -> 'V' (probe)")
        end = time.monotonic() + 0.4
        buf = b""
        while time.monotonic() < end:
            chunk = ser.read(64)
            if chunk:
                buf += chunk
                if b"V1013" in buf:  # firmware version reply
                    return True
    return False


def main():
    args = parse_args()

    ser = serial.Serial()
    ser.port = args.port
    ser.baudrate = args.serial_baud
    ser.bytesize = serial.EIGHTBITS
    ser.parity = serial.PARITY_NONE
    ser.stopbits = serial.STOPBITS_ONE
    ser.timeout = 0.1
    ser.rtscts = False
    ser.dsrdtr = False
    try:
        ser.open()
    except serial.SerialException as e:
        sys.exit(f"cannot open {args.port}: {e}")

    # Keep the auto-reset lines idle; the ESP32 may still reboot on open, so wait.
    try:
        ser.dtr = False
        ser.rts = False
    except OSError:
        pass
    if args.boot_delay > 0:
        time.sleep(args.boot_delay)
    ser.reset_input_buffer()

    # The port-open reset means the firmware may still be booting; wait until it
    # actually answers before handshaking (unless told to skip the handshake).
    if not args.no_open:
        if wait_ready(ser, args.ready_timeout, args.verbose):
            print("# device ready (V1013)")
        else:
            print(f"# WARNING: no response to 'V' within {args.ready_timeout}s "
                  "— firmware not answering on this UART", file=sys.stderr)

    stats = {"tx": 0, "rx": 0, "ack": 0, "nak": 0}
    reader = Reader(ser, stats, args.verbose)
    reader.start()

    # Startup handshake (matches slcand / firmware).
    if not args.no_open:
        send_line(ser, "C", args.verbose)
        time.sleep(0.05)
        send_line(ser, f"S{args.bitrate}", args.verbose)
        time.sleep(0.05)
        send_line(ser, "O", args.verbose)
        time.sleep(0.05)

    kind = "EFF" if args.ext else "SFF"
    print(f"# port {args.port} @ {args.serial_baud} 8N1, CAN {BITRATES[args.bitrate]}")
    if args.listen:
        print("# listen-only: displaying inbound frames (Ctrl-C to stop)")
    else:
        idw = 8 if args.ext else 3
        print(f"# TX id {args.id:0{idw}X} ({kind}) pattern={args.pattern} "
              f"rate={args.rate}/s count={args.count or 'inf'} (Ctrl-C to stop)")

    gen = make_payload_gen(args)
    interval = 1.0 / args.rate if args.rate > 0 else 0.0
    next_t = time.monotonic()
    last_report = next_t

    try:
        while not args.listen:
            payload = b"" if args.rtr else gen()
            line = encode_frame(args.id, payload, ext=args.ext, rtr=args.rtr)
            send_line(ser, line, args.verbose)
            stats["tx"] += 1

            if args.count and stats["tx"] >= args.count:
                break

            if interval:
                next_t += interval
                sleep = next_t - time.monotonic()
                if sleep > 0:
                    time.sleep(sleep)
                else:
                    next_t = time.monotonic()  # we fell behind; resync

            now = time.monotonic()
            if now - last_report >= 1.0:
                print(f"# tx={stats['tx']} rx={stats['rx']} "
                      f"ack={stats['ack']} nak={stats['nak']}", file=sys.stderr)
                last_report = now

        if args.listen:
            while True:
                time.sleep(0.5)
    except KeyboardInterrupt:
        print()
    finally:
        try:
            if not args.no_open:
                send_line(ser, "C")  # close channel / go bus-off
                time.sleep(0.05)
        except serial.SerialException:
            pass
        reader.stop()
        time.sleep(0.15)
        ser.close()
        print(f"# done: tx={stats['tx']} rx={stats['rx']} "
              f"ack={stats['ack']} nak={stats['nak']}")


if __name__ == "__main__":
    main()
