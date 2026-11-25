#!/usr/bin/env python3
"""Print a client's release notes: the section of its CHANGELOG.md for a version.

    clients/release/changelog.py <client> [version]   # default: the declared version

Each clients/<client>/CHANGELOG.md has a section per release, headed
`## <version>` (optionally followed by ` - <date>`). Exits 1 when the
section is missing or empty, so a version cannot ship without notes.
"""
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from version import CLIENTS, DECLARATIONS, version  # noqa: E402


def notes(client: str, release: str) -> str:
    path = CLIENTS / client / "CHANGELOG.md"
    if not path.is_file():
        sys.exit(f"clients/{client}/CHANGELOG.md is missing")
    text = path.read_text(encoding="utf-8")
    heading = re.compile(rf"^## {re.escape(release)}(?: - \d{{4}}-\d{{2}}-\d{{2}})?\s*$", re.M)
    match = heading.search(text)
    if not match:
        sys.exit(f"clients/{client}/CHANGELOG.md has no '## {release}' section")
    rest = text[match.end():]
    end = re.search(r"^## ", rest, re.M)
    body = (rest[: end.start()] if end else rest).strip()
    if not body:
        sys.exit(f"clients/{client}/CHANGELOG.md: the '## {release}' section is empty")
    return body


if __name__ == "__main__":
    if len(sys.argv) not in (2, 3) or sys.argv[1] not in DECLARATIONS:
        sys.exit(f"usage: changelog.py <{'|'.join(DECLARATIONS)}> [version]")
    client = sys.argv[1]
    print(notes(client, sys.argv[2] if len(sys.argv) == 3 else version(client)))
