#!/usr/bin/env python3
"""GitHub-releases stand-in for Friday Night Fights.

Serves the releases API and a Client_Win64 zip so the launcher can install,
preserve saves, and launch without reaching a private repository.

    python3 scripts/mock_release_server.py --port 8899

Then start the launcher with:

    UXA_GITHUB_API_BASE=http://127.0.0.1:8899 \\
    UXA_DATA_DIR=/tmp/uxa-demo \\
    UXA_INSTALL_ROOT=/tmp/uxa-demo/Games \\
    npm run tauri dev

Write a new tag into the version file (printed at startup) and patch again.
"""

from __future__ import annotations

import argparse
import io
import json
import zipfile
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]


def build_zip(tag: str) -> bytes:
    script = """#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
printf '%s\\n' "$1" > "$dir/last-launch.txt"
echo "Friday Night Fights mock client connected to $1"
"""
    notes = f"mock client {tag}\n"
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        archive.writestr("FridayNightFights.exe", script)
        archive.writestr("Engine/README.txt", notes)
        archive.writestr("Config/DefaultGame.ini", "[Game]\n")
    return buf.getvalue()


def build_server_zip(tag: str) -> bytes:
    script = """#!/bin/sh
dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
mkdir -p "$dir/Saved"
printf '%s\\n' '{"playerCount":1}' > "$dir/Saved/uxa-status.json"
printf '%s\\n' "server """ + tag + """" > "$dir/ready.txt"
while IFS= read -r line; do
  printf '%s\\n' "$line" >> "$dir/stdin.log"
  case "$line" in
    quit*) exit 0 ;;
  esac
done
"""
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w", compression=zipfile.ZIP_DEFLATED) as archive:
        info = zipfile.ZipInfo("FridayNightFightsServer.exe")
        info.external_attr = 0o755 << 16
        archive.writestr(info, script)
        archive.writestr("Engine/README.txt", f"mock dedicated server {tag}\n")
    return buf.getvalue()


def load_tag(path: Path) -> str:
    if path.is_file():
        text = path.read_text(encoding="utf-8").strip()
        if text:
            return text
    path.write_text("v0.9.5\n", encoding="utf-8")
    return "v0.9.5"


class Handler(BaseHTTPRequestHandler):
    version_file: Path
    asset_name = "FridayNightFights_0.9.5_Client_Win64.zip"

    def log_message(self, fmt: str, *args: object) -> None:
        print(f"[mock] {self.address_string()} {fmt % args}")

    def do_GET(self) -> None:  # noqa: N802
        path = self.path.split("?", 1)[0]
        if path.endswith("/releases") or "/releases" in path:
            self._releases()
            return
        if path.endswith("/server"):
            self._file(build_server_zip(load_tag(self.version_file)))
            return
        if path.endswith(".zip") or path.endswith("/asset"):
            self._asset()
            return
        self.send_error(404, "not found")

    def _releases(self) -> None:
        tag = load_tag(self.version_file)
        payload = build_zip(tag)
        server_payload = build_server_zip(tag)
        host = self.headers.get("Host", "127.0.0.1")
        body = json.dumps(
            [
                {
                    "tag_name": tag,
                    "name": f"{tag} – The Brawler's Update",
                    "body": "\n".join(
                        [
                            f"## {tag} – The Brawler's Update",
                            "",
                            "- New arena: The Rusty Cage",
                            "- 4 new fighters join the roster",
                            "- Improved netcode & hit registration",
                            "- Performance optimizations",
                            "- Bug fixes and balance tweaks",
                        ]
                    ),
                    "draft": False,
                    "prerelease": False,
                    "html_url": f"http://{host}/release/{tag}",
                    "assets": [
                        {
                            "name": self.asset_name,
                            "url": f"http://{host}/asset",
                            "browser_download_url": f"http://{host}/asset",
                            "size": len(payload),
                            "digest": None,
                        },
                        {
                            "name": "FridayNightFights_Server_Win64.zip",
                            "url": f"http://{host}/server",
                            "browser_download_url": f"http://{host}/server",
                            "size": len(server_payload),
                        },
                    ],
                }
            ]
        ).encode()
        self._send(200, body, "application/json")

    def _asset(self) -> None:
        tag = load_tag(self.version_file)
        payload = build_zip(tag)
        start = 0
        status = 200
        range_header = self.headers.get("Range")
        if range_header and range_header.startswith("bytes="):
            start_text = range_header.removeprefix("bytes=").split("-", 1)[0]
            start = int(start_text or "0")
            if start > len(payload):
                self._send(416, b"", "application/octet-stream")
                return
            payload = payload[start:]
            status = 206
        self._send(status, payload, "application/octet-stream")

    def _file(self, payload: bytes) -> None:
        start = 0
        status = 200
        range_header = self.headers.get("Range")
        if range_header and range_header.startswith("bytes="):
            start_text = range_header.removeprefix("bytes=").split("-", 1)[0]
            start = int(start_text or "0")
            if start > len(payload):
                self._send(416, b"", "application/octet-stream")
                return
            payload = payload[start:]
            status = 206
        self._send(status, payload, "application/octet-stream")

    def _send(self, status: int, body: bytes, content_type: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Accept-Ranges", "bytes")
        self.end_headers()
        self.wfile.write(body)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8899)
    parser.add_argument(
        "--version-file",
        default=str(ROOT / ".uxa-dev" / "mock-version.txt"),
        help="Tag served as the latest Friday Night Fights release",
    )
    args = parser.parse_args()
    version_file = Path(args.version_file)
    version_file.parent.mkdir(parents=True, exist_ok=True)
    tag = load_tag(version_file)
    Handler.version_file = version_file
    server = ThreadingHTTPServer((args.host, args.port), Handler)
    print(f"mock Friday Night Fights releases at http://{args.host}:{args.port}")
    print(f"current tag {tag} (edit {version_file} to publish an update)")
    server.serve_forever()


if __name__ == "__main__":
    main()
