#!/usr/bin/env python3
"""TLS proxy between plain TCP (slcand) and a can_router TLS relay.

slcand has no TLS / pubkey-pinning / token support, so this proxy bridges it:

    slcand  --raw TCP-->  this proxy  --TLS(+token,+pin)-->  can_router  <-->  bridge

It listens on a local TCP port; for each accepted connection it opens a TLS
connection to the relay (pinning the relay's public key from the token URL),
sends the `H<token>\\r` auth line, then relays bytes verbatim in both directions
(it never parses SLCAN — slcand and the bridge speak end-to-end).

Usage
-----
  python slcan_tls_proxy.py \\
      'tls://192.168.1.21:9996/?pubkey=04..&token=84..'  -l 127.0.0.1:2000

  # then, in another shell / on the host:
  slcand -o -s6 -t 127.0.0.1:2000 slcan0   # (FreeBSD slcand, patched TCP client)

Pinning is mandatory unless --insecure is given.
"""

import argparse
import ipaddress
import socket
import ssl
import sys
import threading

BEL = 0x07


# --------------------------------------------------------------------------- #
# SLCAN pretty-printing (verbose mode only; the relay stays byte-transparent)
# --------------------------------------------------------------------------- #
def _decode_frame(line):
    if not line or line[0] not in "tTrR":
        return None
    t = line[0]
    ext, rtr = t in "TR", t in "rR"
    idlen = 8 if ext else 3
    try:
        can_id = int(line[1:1 + idlen], 16)
        dlc = int(line[1 + idlen], 16)
        data = b""
        if not rtr:
            hexb = line[2 + idlen:2 + idlen + dlc * 2]
            if len(hexb) < dlc * 2:
                return None
            data = bytes.fromhex(hexb)
        return (can_id, ext, rtr, dlc, data)
    except (ValueError, IndexError):
        return None


def _pretty(label, line):
    """Render one SLCAN line (bytes, no CR) for display, or None to skip."""
    if not line:
        return None  # bare CR (ack); skip
    try:
        s = line.decode("ascii")
    except UnicodeDecodeError:
        return f"{label} <{line!r}>"
    if s[0] == "H":
        return f"{label} H<token redacted>"
    f = _decode_frame(s)
    if f:
        can_id, ext, rtr, dlc, data = f
        ids = f"{can_id:0{8 if ext else 3}X}"
        kind = "EFF" if ext else "SFF"
        body = f"remote len {dlc}" if rtr else " ".join(f"{b:02X}" for b in data)
        return f"{label} {ids:<8} [{dlc}] {kind}  {body}"
    return f"{label} {s}"  # command / response / relay ping


def _observe(acc, data, label):
    """Feed relayed bytes into a per-direction line accumulator and print them."""
    for byte in data:
        if byte == 0x0D:  # CR ends a line
            msg = _pretty(label, bytes(acc))
            acc.clear()
            if msg is not None:
                print(msg)
        elif byte == BEL:
            msg = _pretty(label, bytes(acc))
            acc.clear()
            if msg is not None:
                print(msg)
            print(f"{label} BEL")
        elif byte != 0x0A:
            acc.append(byte)


# --------------------------------------------------------------------------- #
# Token URL + public-key pinning (same scheme as slcan_router_tool.py)
# --------------------------------------------------------------------------- #
def parse_url(url):
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
    return {"host": host, "port": port,
            "pubkey": params.get("pubkey"), "token": params.get("token")}


def extract_p256_pubkey(der):
    """Find the uncompressed P-256 point (`04`+X+Y) in a DER cert's SPKI."""
    i = der.find(b"\x03\x42\x00\x04")
    if i < 0:
        return None
    key = der[i + 3:i + 3 + 65]
    return key if len(key) == 65 else None


def tls_connect(host, port, pin_hex, timeout, insecure):
    """TLS-connect and pin the relay's public key.

    can_router presents a self-signed P-256 cert with NO CA (see DESIGN.md), so
    CA/hostname verification is disabled (CERT_NONE) and the server is instead
    authenticated by pinning its key: the cert key must equal the URL `pubkey`.
    The TLS handshake proves the server holds that key, so a matched pin == an
    authenticated server. Pinning is mandatory unless `insecure`.
    """
    if not pin_hex and not insecure:
        sys.exit("no pubkey in URL: refusing without server authentication "
                 "(add ?pubkey=<hex>, or pass --insecure).")
    ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_CLIENT)
    ctx.check_hostname = False
    ctx.verify_mode = ssl.CERT_NONE  # intentional: no CA; key pinned below
    try:
        ctx.minimum_version = ssl.TLSVersion.TLSv1_2
    except (AttributeError, ValueError):
        pass
    try:
        sni = None if ipaddress.ip_address(host) else host
    except ValueError:
        sni = host

    raw = socket.create_connection((host, port), timeout=timeout)
    ssock = ctx.wrap_socket(raw, server_hostname=sni)
    ssock.settimeout(None)  # blocking for the relay loop

    der = ssock.getpeercert(binary_form=True)
    server_key = extract_p256_pubkey(der) if der else None
    if server_key is None:
        ssock.close()
        raise ConnectionError("could not extract a P-256 key from the server cert")
    if pin_hex:
        try:
            expected = bytes.fromhex(pin_hex)
        except ValueError:
            ssock.close()
            sys.exit(f"url pubkey is not hex: {pin_hex!r}")
        if server_key != expected:
            ssock.close()
            raise ConnectionError(
                f"PUBKEY MISMATCH expected {expected.hex().upper()} "
                f"got {server_key.hex().upper()}")
    return ssock


# --------------------------------------------------------------------------- #
# Relay
# --------------------------------------------------------------------------- #
def pump(src, dst, both, label=None):
    """Copy src -> dst until EOF/error, then unblock the peer direction.

    If `label` is set (verbose mode), the relayed bytes are also parsed into SLCAN
    lines and printed — purely observational; the relay stays byte-transparent.
    """
    acc = bytearray() if label else None
    try:
        while True:
            data = src.recv(4096)
            if not data:
                break
            dst.sendall(data)  # transparent relay first
            if label:
                _observe(acc, data, label)
    except OSError:
        pass
    finally:
        for s in both:
            try:
                s.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass


def handle(client, peer_addr, u, args):
    print(f"# client {peer_addr} connected; opening TLS to "
          f"{u['host']}:{u['port']}")
    try:
        server = tls_connect(u["host"], u["port"], u["pubkey"],
                             args.connect_timeout, args.insecure)
    except (OSError, ConnectionError) as e:
        print(f"# TLS connect failed: {e}", file=sys.stderr)
        client.close()
        return

    # Authenticate to the relay (consumed by it; silent on success).
    try:
        server.sendall(b"H" + u["token"].encode("ascii") + b"\r")
    except OSError as e:
        print(f"# auth send failed: {e}", file=sys.stderr)
        server.close()
        client.close()
        return
    print("# TLS up, token sent — relaying")

    both = (client, server)
    # client->server = host->bus (TX); server->client = bus->host (RX).
    tx_label = "TX" if args.verbose else None
    rx_label = "RX" if args.verbose else None
    t1 = threading.Thread(target=pump, args=(client, server, both, tx_label), daemon=True)
    t2 = threading.Thread(target=pump, args=(server, client, both, rx_label), daemon=True)
    t1.start()
    t2.start()
    t1.join()
    t2.join()
    for s in both:
        try:
            s.close()
        except OSError:
            pass
    print(f"# client {peer_addr} disconnected")


def parse_args():
    p = argparse.ArgumentParser(
        description="TLS proxy: plain TCP (slcand) <-> can_router TLS relay.",
        formatter_class=argparse.ArgumentDefaultsHelpFormatter,
    )
    p.add_argument("url", help="tls://host:port/?pubkey=<hex>&token=<hex>")
    p.add_argument("-l", "--listen", default="127.0.0.1:2000",
                   help="local [host:]port to listen on for slcand")
    p.add_argument("--connect-timeout", type=float, default=10.0,
                   help="TCP/TLS connect timeout to the relay")
    p.add_argument("--insecure", action="store_true",
                   help="allow connecting without a pinned pubkey (NOT authenticated)")
    p.add_argument("-v", "--verbose", action="store_true",
                   help="print every relayed frame/line (TX = host->bus, RX = bus->host)")
    args = p.parse_args()
    args.parsed = parse_url(args.url)
    if not args.parsed["token"]:
        p.error("URL has no token=; the relay requires one")
    if ":" in args.listen:
        h, _, port = args.listen.rpartition(":")
        args.listen_host = h or "127.0.0.1"
    else:
        args.listen_host, port = "127.0.0.1", args.listen
    try:
        args.listen_port = int(port)
    except ValueError:
        p.error(f"bad --listen port: {port!r}")
    return args


def main():
    args = parse_args()
    u = args.parsed

    lsock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    lsock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    try:
        lsock.bind((args.listen_host, args.listen_port))
    except OSError as e:
        sys.exit(f"cannot bind {args.listen_host}:{args.listen_port}: {e}")
    lsock.listen(1)
    pin = "pinned" if u["pubkey"] else "INSECURE (no pin)"
    print(f"# listening on {args.listen_host}:{args.listen_port} -> "
          f"tls://{u['host']}:{u['port']} [{pin}]")
    print(f"#   slcand -o -s6 -t {args.listen_host}:{args.listen_port} slcan0")

    try:
        while True:
            client, addr = lsock.accept()  # one slcand at a time (relay is 1:1)
            client.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
            try:
                handle(client, f"{addr[0]}:{addr[1]}", u, args)
            except Exception as e:  # keep the listener alive
                print(f"# session error: {e}", file=sys.stderr)
                try:
                    client.close()
                except OSError:
                    pass
    except KeyboardInterrupt:
        print()
    finally:
        lsock.close()


if __name__ == "__main__":
    main()
