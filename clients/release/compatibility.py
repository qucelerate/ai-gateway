#!/usr/bin/env python3
"""Check a release's compatibility results and copy them into its artifacts.

    clients/release/compatibility.py <client> <version> <results> <gateways.txt> <dist>

<results> holds one compatibility-<gateway>/compatibility.json per gateway
version, as clients-compatibility.yml keeps them. For every gateway listed in
<gateways.txt>, the result must record this client, at this version, as
passed; it is then copied to <dist>/compatibility-<gateway>.json. Exits 1,
naming each gateway, when a result is missing or does not show a pass.
"""
import json
import shutil
import sys
from pathlib import Path


def gateways(listing: Path) -> list[str]:
    names = []
    for line in listing.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            names.append(line)
    return names


def main(client: str, version: str, results: Path, listing: Path, dist: Path) -> None:
    problems = []
    for gateway in gateways(listing):
        path = results / f"compatibility-{gateway}" / "compatibility.json"
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            problems.append(f"gateway {gateway}: no readable result ({error})")
            continue
        outcome = record.get("clients", {}).get(client)
        if not outcome:
            problems.append(f"gateway {gateway}: {client} was not run")
        elif outcome.get("version") != version:
            problems.append(f"gateway {gateway}: {client} ran at {outcome.get('version')}, not {version}")
        elif outcome.get("passed") is not True:
            problems.append(f"gateway {gateway}: {client} failed: {outcome.get('detail', 'no detail')}")
        else:
            shutil.copyfile(path, dist / f"compatibility-{gateway}.json")
            print(f"gateway {gateway} ({record.get('gateway', 'unknown')}): {client} {version} passed")
    if problems:
        sys.exit("\n".join(problems))


if __name__ == "__main__":
    if len(sys.argv) != 6:
        sys.exit("usage: compatibility.py <client> <version> <results> <gateways.txt> <dist>")
    main(sys.argv[1], sys.argv[2], *map(Path, sys.argv[3:]))
