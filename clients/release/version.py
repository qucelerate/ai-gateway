#!/usr/bin/env python3
"""Print a client's version, after checking every place it is declared agrees.

    clients/release/version.py <rust|python|go|java|c>

Each client states its version in its manifest and again where it builds its
User-Agent; a release must not ship two different numbers. Exits 1 with the
disagreeing declarations otherwise.
"""
import re
import sys
from pathlib import Path

CLIENTS = Path(__file__).resolve().parent.parent

# (file, pattern whose first group is the version) per declaration.
DECLARATIONS = {
    "rust": [("rust/Cargo.toml", r'(?m)^version = "([^"]+)"')],
    "python": [
        ("python/pyproject.toml", r'(?m)^version = "([^"]+)"'),
        ("python/src/qucelerate_ai_gateway/__init__.py", r'(?m)^__version__ = "([^"]+)"'),
    ],
    "go": [("go/client.go", r'(?m)^const Version = "([^"]+)"')],
    "java": [
        ("java/pom.xml", r"</artifactId>\s*<version>([^<]+)</version>"),
        ("java/src/main/java/com/qucelerate/aigateway/GatewayClient.java",
         r'static final String VERSION = "([^"]+)"'),
    ],
    "c": [
        ("c/include/gateway/gateway.h", r'(?m)^#define GW_VERSION_STRING "([^"]+)"'),
        ("c/include/gateway/gateway.h",
         r"(?ms)^#define GW_VERSION_MAJOR (\d+)\s*$.*?^#define GW_VERSION_MINOR (\d+)\s*$.*?^#define GW_VERSION_PATCH (\d+)\s*$"),
    ],
}

SEMVER = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?$")


def version(client: str) -> str:
    found = []
    for file, pattern in DECLARATIONS[client]:
        match = re.search(pattern, (CLIENTS / file).read_text(encoding="utf-8"))
        if not match:
            sys.exit(f"{file}: no version declaration matching {pattern!r}")
        # A declaration of the three numbers alone (the C header's MAJOR,
        # MINOR and PATCH macros) carries no pre-release suffix.
        found.append((file, ".".join(match.groups()), len(match.groups()) > 1))
    versions = {value for _, value, partial in found if not partial}
    disagree = len(versions) != 1 or any(
        partial and value != next(iter(versions)).split("-", 1)[0] for _, value, partial in found)
    if disagree:
        sys.exit(f"{client}: versions disagree: " + ", ".join(f"{f}={v}" for f, v, _ in found))
    (value,) = versions
    if not SEMVER.match(value):
        sys.exit(f"{client}: {value!r} is not a semantic version")
    return value


if __name__ == "__main__":
    if len(sys.argv) != 2 or sys.argv[1] not in DECLARATIONS:
        sys.exit(f"usage: version.py <{'|'.join(DECLARATIONS)}>")
    print(version(sys.argv[1]))
