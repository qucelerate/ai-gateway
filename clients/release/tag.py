#!/usr/bin/env python3
"""Check a client release tag and print what it releases, as GitHub outputs.

    clients/release/tag.py client-<client>-v<version>

The tag must name a client and the version every declaration of it agrees
on (version.py), and the client's CHANGELOG.md must have that version's
notes (changelog.py). Prints `client=`, `version=` and `prerelease=` lines,
and for the Go client `go_module_tag=clients/go/v<version>`.

A Go module proxy only takes `clients/go/v<version>` tags as versions of the
Go module (it lives in the clients/go subdirectory), so a `client-go-v*` tag
alone never publishes it. In the public repository the release workflow
(release-clients.yml) creates that second tag on the same commit and asks
proxy.golang.org for the version. Never push one in the private
repository: the module path names the public one.
"""
import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from changelog import notes  # noqa: E402
from version import DECLARATIONS, SEMVER, version  # noqa: E402

if __name__ == "__main__":
    if len(sys.argv) != 2:
        sys.exit("usage: tag.py client-<client>-v<version>")
    tag = sys.argv[1]
    match = re.fullmatch(rf"client-({'|'.join(DECLARATIONS)})-v(.+)", tag)
    if not match or not SEMVER.match(match.group(2)):
        sys.exit(f"{tag!r} is not a client release tag (client-<{'|'.join(DECLARATIONS)}>-v<semver>)")
    client, tagged = match.groups()
    declared = version(client)
    if tagged != declared:
        sys.exit(f"{tag}: the tag says {tagged}, but clients/{client} declares {declared}")
    notes(client, tagged)
    print(f"client={client}")
    print(f"version={tagged}")
    print(f"prerelease={'true' if '-' in tagged else 'false'}")
    if client == "go":
        print(f"go_module_tag=clients/go/v{tagged}")
