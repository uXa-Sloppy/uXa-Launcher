#!/usr/bin/env python3
"""Pre-alpha account server.

Reads and writes invite_keys.txt and accounts.txt in one directory. The game
client posts sign-ups here. The dedicated game server (UDP 7797) is a different
process. This process must not listen on 7777 or 7797.

    python Tools/fnf_account_server.py --dir Saved/Accounts --host 127.0.0.1 --port 7798

POST /v1/signup  {"key","name","password"}
GET  /v1/health
"""

from __future__ import annotations

import argparse
import json
import os
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from fnf_account_store import AccountStore

GAME_PORTS = (7777, 7797)
MAX_BODY = 4096


def check_port(port: int) -> None:
    if port in GAME_PORTS or port < 1 or port > 65535:
        print("The account server cannot listen on port %s. Use TCP 7798." % port, file=sys.stderr)
        raise SystemExit(2)


def make_handler(store: AccountStore):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def do_GET(self):
            if self.path.split("?", 1)[0] != "/v1/health":
                self._send(404, {"ok": False, "error": "not_found", "message": "Not found."})
                return
            self._send(200, {"ok": True, "error": "", "message": ""})

        def do_POST(self):
            if self.path.split("?", 1)[0] != "/v1/signup":
                self._send(404, {"ok": False, "error": "not_found", "message": "Not found."})
                return
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                length = -1
            if length < 0 or length > MAX_BODY:
                self._send(400, {"ok": False, "error": "bad_name", "message": "Account name can only use letters, numbers and _."})
                return
            raw = self.rfile.read(length) if length else b""
            try:
                payload = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError):
                self._send(400, {"ok": False, "error": "invalid_key", "message": "Invalid invite key."})
                return
            if not isinstance(payload, dict):
                self._send(400, {"ok": False, "error": "invalid_key", "message": "Invalid invite key."})
                return
            result = store.signup(
                str(payload.get("key", "")),
                str(payload.get("name", "")),
                str(payload.get("password", "")),
            )
            code = 200 if result.get("ok") else 400
            drop = self.headers.get("X-FNF-Drop", "")
            if os.environ.get("FNF_ACCOUNT_TEST_HOOKS") == "1" and drop == "after-commit" and result.get("ok"):
                self.close_connection = True
                print("signup name=%s result=dropped" % payload.get("name", ""), file=sys.stderr)
                return
            name = payload.get("name", "")
            print("signup name=%s result=%s" % (name, result.get("error") or "ok"), file=sys.stderr)
            self._send(code, result)

        def _send(self, status, obj):
            data = json.dumps(obj, separators=(",", ":")).encode("utf-8")
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def log_message(self, fmt, *args):
            # Default logging would only show the request line. Result lines are printed above.
            return

    return Handler


def serve(directory: str, host: str, port: int):
    check_port(port)
    store = AccountStore(directory)
    os.makedirs(directory, exist_ok=True)
    httpd = ThreadingHTTPServer((host, port), make_handler(store))
    print("account server listening on %s:%s (files in %s)" % (host, port, os.path.abspath(directory)), file=sys.stderr)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()


def main(argv=None):
    parser = argparse.ArgumentParser(description="Friday Night Fights account server.")
    parser.add_argument("--dir", default="Saved/Accounts", help="Directory with invite_keys.txt and accounts.txt.")
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=7798)
    args = parser.parse_args(argv)
    serve(args.dir, args.host, args.port)
    return 0


if __name__ == "__main__":
    sys.exit(main())
