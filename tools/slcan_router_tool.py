#!/usr/bin/env python3
"""SLCAN test tool over a can_router TLS relay.

Like slcan_tool.py, but instead of a serial port / raw TCP it connects to a
can_router relay (see ../can_router/DESIGN.md) over TLS, pins the relay's
public key, and authenticates with a token. Once paired with the bridge on the
other side it is a transparent SLCAN byte pipe, so it then behaves exactly like
slcan_tool: generate frames at a rate and display inbound frames.

Target is the token URL the can_router website hands out:

  tls://<host>:<port>/?pubkey=<server-pubkey-hex>&token=<token-hex>

* pubkey — relay's pinned P-256 key (uncompressed SEC1 `04`+X+Y, 130 hex). If
  given, the server cert's key must match it, else we abort (MITM guard).
* token  — 64-hex auth/access token; sent as `H<token>\\r` right after TLS.

Examples
--------
  python slcan_router_tool.py 'tls://192.168.1.21:9996/?pubkey=04..&token=84..' \\
      -b 6 -i 123 -d DEADBEEF -r 50

  python slcan_router_tool.py 'tls://host:9996/?pubkey=04..&token=84..' --listen
"""

import argparse
import ipaddress
import random
import socket
import ssl
import sys
import threading
import time

CR = b"\r"
BEL = 0x07

BITRATES = {
    0: "10k", 1: "20k", 2: "50k", 3: "100k", 4: "125k",
    5: "250k", 6: "500k", 7: "800k", 8: "1M",
}


# --------------------------------------------------------------------------- #
# SLCAN frame encode / decode (identical to slcan_tool.py)
# --------------------------------------------------------------------------- #
def encode_frame(can_id, data, ext=False, rtr=False):
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
        return {"id": can_id, "ext": ext, "rtr": rtr, "dlc": dlc, "data": data}
    except (ValueError, IndexError):
        return None


def fmt_frame(f):
    width = 8 if f["ext"] else 3
    ids = f"{f['id']:0{width}X}"
    if f["rtr"]:
        body = f"remote len {f['dlc']}"
    else:
        body = " ".join(f"{b:02X}" for b in f["data"])
    kind = "EFF" if f["ext"] else "SFF"
    return f"{ids:<8} [{f['dlc']}] {kind}  {body}"


def make_payload_gen(args):
    if args.pattern == "fixed":
        try:
            fixed = bytes.fromhex(args.data)
        except ValueError:
            sys.exit(f"--data must be hex bytes, got {args.data!r}")
        length = args.len if args.len is not None else len(fixed)
        fixed = (fixed + bytes(8))[:length]
        return lambda: fixed

    length = args.len if args.len is not None else 8
    length = max(0, min(8, length))
    if args.pattern == "random":
        return lambda: bytes(random.randrange(256) for _ in range(length))

    state = {"n": 0}

    def inc():
        if length == 0:
            return b""
        val = state["n"] & ((1 << (8 * length)) - 1)
        state["n"] += 1
        return val.to_bytes(length, "big")

    return inc


# --------------------------------------------------------------------------- #
# URL + pubkey pinning
# --------------------------------------------------------------------------- #
def parse_url(url):
    """Parse tls://host:port/?pubkey=<hex>&token=<hex> -> dict."""
    if "://" not in url:
        sys.exit(f"bad URL (need tls://host:port/...): {url!r}")
    scheme, rest = url.split("://", 1)
    if scheme != "tls":
        sys.exit(f"only tls:// is supported, got {scheme!r}")
    before_q, _, query = rest.partition("?")
    authority = before_q.rstrip("/")
    if ":" not in authority:
        sys.exit(f"missing :port in URL: {url!r}")
    host, port_str = authority.rsplit(":", 1)
    try:
        port = int(port_str)
    except ValueError:
        sys.exit(f"bad port in URL: {port_str!r}")
    params = {}
    for kv in query.split("&"):
        if "=" in kv:
            k, v = kv.split("=", 1)
            params[k] = v
    return {
        "host": host,
        "port": port,
        "pubkey": params.get("pubkey"),
        "token": params.get("token"),
    }


def extract_p256_pubkey(der):
    """Find the uncompressed P-256 point (`04`+X+Y) in a DER cert's SPKI."""
    i = der.find(b"\x03\x42\x00\x04")  # BIT STRING(66) 0-unused 04
    if i < 0:
        return None
    key = der[i + 3:i + 3 + 65]
    return key if len(key) == 65 else None


def tls_connect(host, port, pin_hex, timeout, insecure):
    """TLS-connect, pin the server pubkey, return the wrapped socket.

    can_router presents a self-signed P-256 cert with NO CA (see DESIGN.md), so
    authentication is by *public-key pinning*: CA/hostname verification is disabled
    (CERT_NONE) and instead we require the server cert's key to equal the pinned
    `pubkey` from the token URL. The TLS handshake still proves the server holds
    that key (CertificateVerify), so a matched pin == an authenticated server.
    Pinning is mandatory unless --insecure is given.
    """
    if not pin_hex and not insecure:
        sys.exit(
            "no pubkey in URL: refusing to connect without server authentication.\n"
            "Add ?pubkey=<hex> (from the token URL), or pass --insecure to override."
        )
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # intentional: no CA; key is pinned below
    try:
        ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    except (AttributeError, ValueError):
        pass

    try:
        sni = None if ipaddress.ip_address(host) else host
    except ValueError:
        sni = host  # hostname, not an IP

    raw = socket.create_connection((host, port), timeout=timeout)
    ssock = ctx.wrap_socket(raw, server_hostname=sni)

    der = ssock.getpeercert(binary_form=True)
    if not der:
        ssock.close()
        sys.exit("could not read server certificate")
    server_key = extract_p256_pubkey(der)
    if server_key is None:
        ssock.close()
        sys.exit("could not extract a P-256 key from the server certificate")
    print(f"# server pubkey {server_key.hex().upper()}")

    if pin_hex:
        try:
            expected = bytes.fromhex(pin_hex)
        except ValueError:
            ssock.close()
            sys.exit(f"--pubkey/url pubkey is not hex: {pin_hex!r}")
        if server_key != expected:
            ssock.close()
            sys.exit(
                "PUBKEY MISMATCH — refusing to continue\n"
                f"  expected {expected.hex().upper()}\n"
                f"  got      {server_key.hex().upper()}"
            )
        print("# server pubkey pinned OK")
    else:
        print("# WARNING: --insecure: server NOT authenticated (encrypt-only, MITM-exposed)")
    return ssock


# --------------------------------------------------------------------------- #
# TLS link: presents the read()/write() interface the rest of the tool uses
# --------------------------------------------------------------------------- #
class TlsLink:
    def __init__(self, ssock, read_timeout=0.2):
        self.sock = ssock
        self.sock.settimeout(read_timeout)
        self.read_timeout = read_timeout

    def read(self, n):
        """bytes on data, b'' on idle timeout, None on EOF/error."""
        try:
            data = self.sock.recv(n)
        except socket.timeout:
            return b""
        except OSError:
            return None
        return data if data else None

    def write(self, data):
        self.sock.sendall(data)

    def reset_input_buffer(self):
        self.sock.settimeout(0.0)
        try:
            while self.sock.recv(65536):
                pass
        except (BlockingIOError, ssl.SSLWantReadError, OSError):
            pass
        finally:
            self.sock.settimeout(self.read_timeout)

    def close(self):
        try:
            self.sock.close()
        except OSError:
            pass


# --------------------------------------------------------------------------- #
# Reader thread
# --------------------------------------------------------------------------- #
class Reader(threading.Thread):
    def __init__(self, link, stats, verbose):
        super().__init__(daemon=True)
        self.link = link
        self.stats = stats
        self.verbose = verbose
        self._stop = threading.Event()

    def run(self):
        buf = bytearray()
        while not self._stop.is_set():
            chunk = self.link.read(256)
            if chunk is None:
                break  # connection closed
            if not chunk:
                continue  # idle
            for byte in chunk:
                if byte == 0x0D:  # CR
                    self._dispatch(bytes(buf))
                    buf.clear()
                elif byte == BEL:
                    self.stats["nak"] += 1
                    if self.verbose:
                        print("  <- BEL (nak)")
                elif byte != 0x0A:
                    buf.append(byte)

    def _dispatch(self, raw):
        if not raw:
            self.stats["ack"] += 1
            return
        line = raw.decode("ascii", "replace")
        if line in ("z", "Z"):
            self.stats["ack"] += 1
            return
        if line == ".":
            if self.verbose:
                print("  <- . (relay ping)")
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
        description="SLCAN test tool over a can_router TLS relay.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument("url", help="tls://host:port/?pubkey=<hex>&token=<hex>")
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
                   help="payload length in bytes (0-8)")
    p.add_argument("-r", "--rate", type=float, default=10.0,
                   help="frames per second to generate (0 = as fast as possible)")
    p.add_argument("-c", "--count", type=int, default=0,
                   help="number of frames to send (0 = unlimited)")
    p.add_argument("--listen", action="store_true",
                   help="receive only: open the channel but do not transmit")
    p.add_argument("--no-open", action="store_true",
                   help="skip the C/S/O startup sequence")
    p.add_argument("--ready-timeout", type=float, default=30.0,
                   help="max seconds to wait for the bridge (via the relay) to "
                        "answer 'V' before handshaking")
    p.add_argument("--connect-timeout", type=float, default=10.0,
                   help="TCP/TLS connect timeout")
    p.add_argument("--insecure", action="store_true",
                   help="allow connecting without a pinned pubkey (NOT authenticated)")
    p.add_argument("-v", "--verbose", action="store_true",
                   help="show acks/naks, relay pings, non-frame lines, and TX")
    args = p.parse_args()
    args.parsed = parse_url(args.url)
    if not args.parsed["token"]:
        p.error("URL has no token=; the relay requires one")
    try:
        args.id = int(args.id, 16)
    except ValueError:
        p.error(f"--id must be hex, got {args.id!r}")
    return args


def send_line(link, text, verbose=False):
    link.write(text.encode("ascii") + CR)
    if verbose:
        print(f"  -> {text!r}")


def wait_ready(link, timeout, verbose=False):
    """Probe with 'V' until the bridge (via the relay) answers 'V1013'."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        link.reset_input_buffer()
        link.write(b"V\r")
        if verbose:
            print("  -> 'V' (probe)")
        end = time.monotonic() + 0.5
        buf = b""
        while time.monotonic() < end:
            chunk = link.read(64)
            if chunk is None:
                return False  # closed
            if chunk:
                buf += chunk
                if b"V1013" in buf:
                    return True
    return False


def main():
    args = parse_args()
    u = args.parsed

    try:
        ssock = tls_connect(u["host"], u["port"], u["pubkey"], args.connect_timeout,
                            args.insecure)
    except OSError as e:
        sys.exit(f"connect to {u['host']}:{u['port']} failed: {e}")
    link = TlsLink(ssock)

    # Authenticate to the relay: H<token>\r (consumed by the relay, silent on OK).
    send_line(link, "H" + u["token"], args.verbose)

    # Wait until the bridge on the far side answers (it may pair after us).
    if not args.no_open:
        if wait_ready(link, args.ready_timeout, args.verbose):
            print("# bridge ready (V1013)")
        else:
            print(f"# WARNING: no 'V1013' within {args.ready_timeout}s "
                  "(bridge offline / wrong token?)", file=sys.stderr)

    stats = {"tx": 0, "rx": 0, "ack": 0, "nak": 0}
    reader = Reader(link, stats, args.verbose)
    reader.start()

    if not args.no_open:
        send_line(link, "C", args.verbose)
        time.sleep(0.05)
        send_line(link, f"S{args.bitrate}", args.verbose)
        time.sleep(0.05)
        send_line(link, "O", args.verbose)
        time.sleep(0.05)

    kind = "EFF" if args.ext else "SFF"
    print(f"# {u['host']}:{u['port']} TLS, CAN {BITRATES[args.bitrate]}")
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
            send_line(link, encode_frame(args.id, payload, ext=args.ext, rtr=args.rtr),
                      args.verbose)
            stats["tx"] += 1
            if args.count and stats["tx"] >= args.count:
                break
            if interval:
                next_t += interval
                sleep = next_t - time.monotonic()
                if sleep > 0:
                    time.sleep(sleep)
                else:
                    next_t = time.monotonic()
            now = time.monotonic()
            if now - last_report >= 1.0:
                print(f"# tx={stats['tx']} rx={stats['rx']} "
                      f"ack={stats['ack']} nak={stats['nak']}", file=sys.stderr)
                last_report = now

        if args.listen:
            while reader.is_alive():
                time.sleep(0.5)
    except KeyboardInterrupt:
        print()
    finally:
        try:
            if not args.no_open:
                send_line(link, "C")
                time.sleep(0.05)
        except OSError:
            pass
        reader.stop()
        time.sleep(0.15)
        link.close()
        print(f"# done: tx={stats['tx']} rx={stats['rx']} "
              f"ack={stats['ack']} nak={stats['nak']}")


if __name__ == "__main__":
    main()
