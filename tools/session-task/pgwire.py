"""Minimal stdlib-only PostgreSQL client (protocol v3, simple query).

Why this exists: session-task runs on BOTH the macOS host and inside the
claude-container, and neither has a Postgres driver (psycopg is not installed
in either; the container has no network at runtime to pip install). The queue
store only needs: connect, authenticate (cleartext / md5 / SCRAM-SHA-256),
run text SQL, read text rows. That is ~200 lines of wire protocol, so we carry
it instead of adding a native dependency to two environments.

Not a general driver: simple-query protocol only, every value comes back as
str/None, callers MUST build SQL with `quote_literal` / dollar-quoting.
"""

import base64
import hashlib
import hmac
import os
import secrets
import socket
import struct
from urllib.parse import unquote, urlparse


class PgError(Exception):
    """Server ErrorResponse or protocol failure."""

    def __init__(self, msg, sqlstate=None):
        super().__init__(msg)
        self.sqlstate = sqlstate


class PgConnError(PgError):
    """Could not connect / authenticate (retryable, fail-loud upstream)."""


def quote_literal(value):
    """SQL string literal safe for standard_conforming_strings=on."""
    s = str(value)
    if "\x00" in s:
        raise ValueError("NUL in SQL literal")
    return "'" + s.replace("'", "''") + "'"


def parse_dsn(dsn):
    u = urlparse(dsn)
    if u.scheme not in ("postgresql", "postgres"):
        raise ValueError("DSN must be postgresql://user:pass@host:port/db")
    return {
        "user": unquote(u.username or ""),
        "password": unquote(u.password or ""),
        "host": u.hostname or "127.0.0.1",
        "port": u.port or 5432,
        "database": (u.path or "/").lstrip("/") or (u.username or ""),
    }


class PgConnection:
    def __init__(self, dsn, timeout=5.0):
        p = parse_dsn(dsn) if isinstance(dsn, str) else dict(dsn)
        self.params = p
        try:
            self.sock = socket.create_connection(
                (p["host"], p["port"]), timeout=timeout)
        except OSError as exc:
            raise PgConnError(f"connect {p['host']}:{p['port']}: {exc}")
        self.sock.settimeout(30.0)
        self._buf = b""
        try:
            self._startup()
        except PgConnError:
            self.close()
            raise
        except (OSError, struct.error) as exc:
            self.close()
            raise PgConnError(f"startup: {exc}")

    # -- framing ---------------------------------------------------------
    def _recv_exact(self, n):
        while len(self._buf) < n:
            chunk = self.sock.recv(65536)
            if not chunk:
                raise PgConnError("connection closed by server")
            self._buf += chunk
        out, self._buf = self._buf[:n], self._buf[n:]
        return out

    def _read_msg(self):
        t = self._recv_exact(1)
        (ln,) = struct.unpack("!I", self._recv_exact(4))
        return t, self._recv_exact(ln - 4)

    def _send(self, type_byte, payload):
        self.sock.sendall(type_byte + struct.pack("!I", len(payload) + 4)
                          + payload)

    @staticmethod
    def _parse_error(body):
        fields = {}
        for part in body.split(b"\x00"):
            if part:
                fields[chr(part[0])] = part[1:].decode("utf-8", "replace")
        return fields.get("M", "error"), fields.get("C")

    # -- startup / auth --------------------------------------------------
    def _startup(self):
        p = self.params
        kv = (b"user\x00" + p["user"].encode() + b"\x00database\x00"
              + p["database"].encode() + b"\x00application_name\x00"
              b"session-task\x00\x00")
        payload = struct.pack("!I", 196608) + kv
        self.sock.sendall(struct.pack("!I", len(payload) + 4) + payload)
        scram = None
        while True:
            t, body = self._read_msg()
            if t == b"R":
                (code,) = struct.unpack("!I", body[:4])
                if code == 0:
                    continue
                elif code == 3:
                    self._send(b"p", p["password"].encode() + b"\x00")
                elif code == 5:
                    salt = body[4:8]
                    inner = hashlib.md5(
                        (p["password"] + p["user"]).encode()).hexdigest()
                    outer = hashlib.md5(inner.encode() + salt).hexdigest()
                    self._send(b"p", b"md5" + outer.encode() + b"\x00")
                elif code == 10:
                    if b"SCRAM-SHA-256" not in body[4:]:
                        raise PgConnError("server offers no SCRAM-SHA-256")
                    scram = _Scram(p["password"])
                    first = scram.client_first()
                    self._send(b"p", b"SCRAM-SHA-256\x00"
                               + struct.pack("!I", len(first)) + first)
                elif code == 11:
                    self._send(b"p", scram.client_final(body[4:].decode()))
                elif code == 12:
                    scram.verify_server(body[4:].decode())
                else:
                    raise PgConnError(f"unsupported auth method {code}")
            elif t == b"E":
                msg, state = self._parse_error(body)
                raise PgConnError(f"auth/startup failed: {msg}", state)
            elif t == b"Z":
                return
            # S (ParameterStatus), K (BackendKeyData), N: ignore

    # -- queries ---------------------------------------------------------
    def query(self, sql):
        """Run one or more statements; return rows of the LAST result set as
        a list of tuples of str|None. Raises PgError on server error (the
        connection is drained back to ReadyForQuery first)."""
        self._send(b"Q", sql.encode("utf-8") + b"\x00")
        rows, err = [], None
        while True:
            t, body = self._read_msg()
            if t == b"T":
                rows = []  # new result set
            elif t == b"D":
                (n,) = struct.unpack("!H", body[:2])
                off, row = 2, []
                for _ in range(n):
                    (ln,) = struct.unpack("!i", body[off:off + 4])
                    off += 4
                    if ln == -1:
                        row.append(None)
                    else:
                        row.append(body[off:off + ln].decode("utf-8"))
                        off += ln
                rows.append(tuple(row))
            elif t == b"E":
                err = self._parse_error(body)
            elif t == b"Z":
                if err:
                    raise PgError(err[0], err[1])
                return rows
            # C (CommandComplete), I, N, S: ignore

    def close(self):
        try:
            self._send(b"X", b"")
        except Exception:  # noqa: BLE001
            pass
        try:
            self.sock.close()
        except Exception:  # noqa: BLE001
            pass


class _Scram:
    def __init__(self, password):
        self.password = password.encode()
        self.nonce = base64.b64encode(secrets.token_bytes(18)).decode()
        self.first_bare = f"n=,r={self.nonce}"
        self.server_sig = None

    def client_first(self):
        return ("n,," + self.first_bare).encode()

    def client_final(self, server_first):
        attrs = dict(a.split("=", 1) for a in server_first.split(","))
        nonce, salt, iters = attrs["r"], base64.b64decode(attrs["s"]), int(attrs["i"])
        if not nonce.startswith(self.nonce):
            raise PgConnError("SCRAM nonce mismatch")
        salted = hashlib.pbkdf2_hmac("sha256", self.password, salt, iters)
        ckey = hmac.new(salted, b"Client Key", hashlib.sha256).digest()
        stored = hashlib.sha256(ckey).digest()
        without_proof = f"c=biws,r={nonce}"
        auth_msg = f"{self.first_bare},{server_first},{without_proof}".encode()
        csig = hmac.new(stored, auth_msg, hashlib.sha256).digest()
        proof = bytes(a ^ b for a, b in zip(ckey, csig))
        skey = hmac.new(salted, b"Server Key", hashlib.sha256).digest()
        self.server_sig = hmac.new(skey, auth_msg, hashlib.sha256).digest()
        return (without_proof + ",p=" + base64.b64encode(proof).decode()).encode()

    def verify_server(self, server_final):
        attrs = dict(a.split("=", 1) for a in server_final.split(","))
        if "e" in attrs:
            raise PgConnError(f"SCRAM server error: {attrs['e']}")
        if base64.b64decode(attrs.get("v", "")) != self.server_sig:
            raise PgConnError("SCRAM server signature mismatch")


def dollar_quote(text):
    """Dollar-quote `text` with a tag guaranteed absent from it."""
    while True:
        tag = "q" + secrets.token_hex(6)
        if f"${tag}$" not in text:
            return f"${tag}${text}${tag}$"
