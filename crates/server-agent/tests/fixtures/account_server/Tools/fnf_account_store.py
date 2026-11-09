#!/usr/bin/env python3
"""Shared invite-key list and accounts registry for the pre-alpha account server.

Password hashing matches UFNFLocalAccountBackend / FNFCrypto::HashPassword:

    H0 = SHA256(salt || utf8(password))
    Hi = SHA256(H{i-1} || salt)   for i = 1 .. iterations-1

iterations is 20000. The plaintext password and the plaintext invite key are
never written. A key has no expiry: a stopped account server does not use it up.
"""

from __future__ import annotations

import hashlib
import hmac
import os
import secrets
import time
from contextlib import contextmanager
from datetime import datetime, timezone

HASH_ITERATIONS = 20000
KEY_ALPHABET = "ABCDEFGHJKLMNPQRSTUVWXYZ23456789"
ACCOUNT_NAME_MAX = 20
# Name, Name1 and Name2 must all fit in ACCOUNT_NAME_MAX.
INVITE_NAME_MAX = ACCOUNT_NAME_MAX - 1
MIN_NAME_LEN = 3
MIN_PASSWORD_LEN = 6
MAX_PASSWORD_LEN = 64

KEY_FIELDS = (
    "key_sha256",
    "status",
    "created_utc",
    "note",
    "group_id",
    "used_utc",
    "used_name",
    "pending_salt",
    "pending_hash",
)
ACCOUNT_FIELDS = (
    "name",
    "password_salt_hex",
    "password_hash_hex",
    "iterations",
    "group_id",
    "created_utc",
)

KEY_HEADER = (
    "# FNF invite keys v1\n"
    "# key_sha256, status, created_utc, note, group_id, used_utc, used_name, pending_salt, pending_hash\n"
    "# Columns are tab-separated. status is unused, pending, or used.\n"
    "# The plaintext key is not stored. Keys do not expire.\n"
    "# pending means a sign-up started; the same name and password may finish it.\n"
)
ACCOUNT_HEADER = (
    "# FNF accounts v1\n"
    "# name, password_salt_hex, password_hash_hex, iterations, group_id, created_utc\n"
    "# Columns are tab-separated. The plaintext password is not stored.\n"
    "# Hash matches the game: iterated salted SHA-256, 20000 rounds.\n"
    "# group_id is shared by the three accounts created from one invite key.\n"
)


def utc_now():
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


def sha256_hex(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def hash_password(password: str, salt: bytes, iterations: int = HASH_ITERATIONS) -> str:
    if iterations <= 0:
        raise ValueError("iterations")
    digest = hashlib.sha256(salt + password.encode("utf-8")).digest()
    for _ in range(iterations - 1):
        digest = hashlib.sha256(digest + salt).digest()
    return digest.hex()


def normalize_key(key: str) -> str:
    return (key or "").strip().upper()


def key_hash(key: str) -> str:
    return sha256_hex(normalize_key(key).encode("utf-8"))


def trio(name: str):
    return (name, name + "1", name + "2")


def validate_name(name: str):
    if not isinstance(name, str):
        return "Account name can only use letters, numbers and _."
    name = name.strip()
    if len(name) < MIN_NAME_LEN or len(name) > INVITE_NAME_MAX:
        return "Account name must be 3-19 characters."
    for ch in name:
        ok = ("A" <= ch <= "Z") or ("a" <= ch <= "z") or ("0" <= ch <= "9") or ch == "_"
        if not ok:
            return "Account name can only use letters, numbers and _."
    for item in trio(name):
        if len(item) > ACCOUNT_NAME_MAX:
            return "Account name must be 3-19 characters."
    return None


def validate_password(password: str):
    if not isinstance(password, str) or len(password) < MIN_PASSWORD_LEN or len(password) > MAX_PASSWORD_LEN:
        return "Password must be 6-64 characters."
    return None


def _dash(value: str) -> str:
    text = "" if value is None else str(value)
    text = text.replace("\t", " ").replace("\r", " ").replace("\n", " ").strip()
    return text if text else "-"


def _undash(value: str) -> str:
    return "" if value is None or value == "-" else value


def _split_row(line: str, width: int):
    parts = line.rstrip("\n").split("\t")
    while len(parts) < width:
        parts.append("-")
    return [_dash(p) for p in parts[:width]]


def _read_table(path: str, fields):
    rows = []
    if not os.path.isfile(path):
        return rows
    with open(path, "r", encoding="utf-8", newline="\n") as handle:
        for line in handle:
            if not line.strip() or line.startswith("#"):
                continue
            parts = _split_row(line, len(fields))
            rows.append(dict(zip(fields, parts)))
    return rows


def _write_table(path: str, header: str, fields, rows):
    lines = [header.rstrip("\n")]
    for row in rows:
        lines.append("\t".join(_dash(row.get(field, "-")) for field in fields))
    text = "\n".join(lines) + "\n"
    parent = os.path.dirname(path)
    if parent:
        os.makedirs(parent, exist_ok=True)
    tmp = path + ".tmp"
    with open(tmp, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)
        handle.flush()
        os.fsync(handle.fileno())
    os.replace(tmp, path)


def _password_matches(password: str, salt_hex: str, hash_hex: str) -> bool:
    try:
        salt = bytes.fromhex(salt_hex)
    except ValueError:
        return False
    if not salt or not hash_hex or hash_hex == "-":
        return False
    candidate = hash_password(password, salt, HASH_ITERATIONS)
    if len(candidate) != len(hash_hex):
        return False
    return hmac.compare_digest(candidate, hash_hex)


def mint_invite_key(path: str, note: str = "") -> str:
    """Append the hash of a new key. Returns the plaintext key (caller prints it)."""
    directory = os.path.dirname(os.path.abspath(path)) or "."
    os.makedirs(directory, exist_ok=True)
    # Same lock the account server uses when this file lives in its directory.
    with AccountStore(directory)._lock():
        rows = _read_table(path, KEY_FIELDS)
        existing = {row["key_sha256"] for row in rows}
        for _ in range(8):
            raw = "".join(secrets.choice(KEY_ALPHABET) for _ in range(12))
            key = "FNF-%s-%s-%s" % (raw[0:4], raw[4:8], raw[8:12])
            digest = key_hash(key)
            if digest in existing:
                continue
            rows.append({
                "key_sha256": digest,
                "status": "unused",
                "created_utc": utc_now(),
                "note": _dash(note),
                "group_id": "-",
                "used_utc": "-",
                "used_name": "-",
                "pending_salt": "-",
                "pending_hash": "-",
            })
            _write_table(path, KEY_HEADER, KEY_FIELDS, rows)
            return key
    raise RuntimeError("could not mint a unique invite key")


class AccountStore:
    def __init__(self, directory: str):
        self.directory = directory
        self.keys_path = os.path.join(directory, "invite_keys.txt")
        self.accounts_path = os.path.join(directory, "accounts.txt")
        self.lock_path = os.path.join(directory, "accounts.lock")

    @contextmanager
    def _lock(self):
        os.makedirs(self.directory, exist_ok=True)
        handle = open(self.lock_path, "a+b")
        try:
            handle.seek(0, os.SEEK_END)
            if handle.tell() == 0:
                handle.write(b"\0")
                handle.flush()
            handle.seek(0)
            if os.name == "nt":
                import msvcrt
                while True:
                    try:
                        msvcrt.locking(handle.fileno(), msvcrt.LK_LOCK, 1)
                        break
                    except OSError:
                        time.sleep(0.02)
            else:
                import fcntl
                fcntl.flock(handle.fileno(), fcntl.LOCK_EX)
            yield
        finally:
            try:
                if os.name == "nt":
                    import msvcrt
                    handle.seek(0)
                    msvcrt.locking(handle.fileno(), msvcrt.LK_UNLCK, 1)
                else:
                    import fcntl
                    fcntl.flock(handle.fileno(), fcntl.LOCK_UN)
            except OSError:
                pass
            handle.close()

    def signup(self, key: str, name: str, password: str, crash_after=None):
        with self._lock():
            return self._signup_locked(key, name, password, crash_after)

    def _signup_locked(self, key: str, name: str, password: str, crash_after):
        canon = normalize_key(key)
        if not canon:
            return _fail("invalid_key", "Invalid invite key.")
        name = name.strip() if isinstance(name, str) else ""
        name_error = validate_name(name)
        if name_error:
            return _fail("bad_name", name_error)
        password_error = validate_password(password)
        if password_error:
            return _fail("bad_password", password_error)

        keys = _read_table(self.keys_path, KEY_FIELDS)
        accounts = _read_table(self.accounts_path, ACCOUNT_FIELDS)
        digest = key_hash(canon)
        row = next((item for item in keys if item["key_sha256"] == digest), None)
        if row is None:
            return _fail("invalid_key", "Invalid invite key.")

        by_name = {item["name"].lower(): item for item in accounts}
        if row["status"] == "unused":
            for item in trio(name):
                if item.lower() in by_name:
                    return _fail("name_taken", "That name is already taken.")
            salt = secrets.token_bytes(32)
            row["status"] = "pending"
            row["group_id"] = "grp_" + secrets.token_hex(8)
            row["used_name"] = name
            row["pending_salt"] = salt.hex()
            row["pending_hash"] = hash_password(password, salt, HASH_ITERATIONS)
            row["used_utc"] = "-"
            _write_table(self.keys_path, KEY_HEADER, KEY_FIELDS, keys)
            if crash_after == "pending":
                return _fail("crashed", "crashed")
            if not self._ensure_accounts(accounts, by_name, row):
                row["status"] = "unused"
                row["group_id"] = "-"
                row["used_name"] = "-"
                row["pending_salt"] = "-"
                row["pending_hash"] = "-"
                row["used_utc"] = "-"
                _write_table(self.keys_path, KEY_HEADER, KEY_FIELDS, keys)
                return _fail("name_taken", "That name is already taken.")
            _write_table(self.accounts_path, ACCOUNT_HEADER, ACCOUNT_FIELDS, accounts)
            if crash_after == "accounts":
                return _fail("crashed", "crashed")
            row["status"] = "used"
            row["used_utc"] = utc_now()
            _write_table(self.keys_path, KEY_HEADER, KEY_FIELDS, keys)
            return _ok(row)

        if row["status"] not in ("pending", "used"):
            return _fail("invalid_key", "Invalid invite key.")
        if row["used_name"].lower() != name.lower():
            return _fail("used_key", "That invite key has already been used.")
        if not _password_matches(password, row.get("pending_salt", ""), row.get("pending_hash", "")):
            return _fail("used_key", "That invite key has already been used.")
        if not self._ensure_accounts(accounts, by_name, row):
            return _fail("name_taken", "That name is already taken.")
        _write_table(self.accounts_path, ACCOUNT_HEADER, ACCOUNT_FIELDS, accounts)
        if row["status"] != "used":
            row["status"] = "used"
            if _undash(row.get("used_utc", "")) == "":
                row["used_utc"] = utc_now()
            _write_table(self.keys_path, KEY_HEADER, KEY_FIELDS, keys)
        return _ok(row)

    def _ensure_accounts(self, accounts, by_name, row) -> bool:
        names = trio(row["used_name"])
        created = _undash(row.get("used_utc", "")) or utc_now()
        for item in names:
            have = by_name.get(item.lower())
            if have:
                if have.get("group_id") != row["group_id"]:
                    return False
                if have.get("password_salt_hex") != row["pending_salt"] or have.get("password_hash_hex") != row["pending_hash"]:
                    return False
                continue
            record = {
                "name": item,
                "password_salt_hex": row["pending_salt"],
                "password_hash_hex": row["pending_hash"],
                "iterations": str(HASH_ITERATIONS),
                "group_id": row["group_id"],
                "created_utc": created,
            }
            accounts.append(record)
            by_name[item.lower()] = record
        return True


def _fail(code: str, message: str):
    return {"ok": False, "error": code, "message": message, "group": "", "accounts": []}


def _ok(row):
    names = list(trio(row["used_name"]))
    return {
        "ok": True,
        "error": "",
        "message": "",
        "group": row["group_id"],
        "accounts": names,
    }
