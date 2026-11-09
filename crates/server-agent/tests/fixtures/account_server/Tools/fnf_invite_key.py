#!/usr/bin/env python3
"""Mint one pre-alpha invite key.

Prints the plaintext key on stdout and appends only its SHA-256 to the key list.
The Discord bot can read that line and DM it. This script does not send the DM.

    python Tools/fnf_invite_key.py --file Saved/Accounts/invite_keys.txt --note "discordname"
"""

from __future__ import annotations

import argparse
import sys

from fnf_account_store import mint_invite_key


def main(argv=None):
    parser = argparse.ArgumentParser(description="Mint one Friday Night Fights invite key.")
    parser.add_argument(
        "--file",
        default=os_default(),
        help="Key list to append to (default: Saved/Accounts/invite_keys.txt).",
    )
    parser.add_argument("--note", default="", help="Optional note, usually a Discord username.")
    args = parser.parse_args(argv)
    try:
        key = mint_invite_key(args.file, args.note)
    except OSError as exc:
        print("could not write the key list: %s" % exc, file=sys.stderr)
        return 1
    print(key)
    print("appended key hash to %s" % args.file, file=sys.stderr)
    return 0


def os_default():
    return "Saved/Accounts/invite_keys.txt"


if __name__ == "__main__":
    sys.exit(main())
