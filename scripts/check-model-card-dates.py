#!/usr/bin/env python3
"""Fail when a model card fact was read too long ago, or in the future.

Every fact in model-cards/ carries `retrieved_at`: the day its source was
read. Providers change prices, limits, retirement dates and terms, so a fact
older than MAX_AGE_DAYS (default 180, about two provider release cycles) is
due for a re-read: open its source_url, update the value if it changed, and
set retrieved_at to today (model-cards/README.md, "Refreshing").

Usage: scripts/check-model-card-dates.py [DIR] [--max-age-days N] [--today YYYY-MM-DD]
The MODEL_CARD_MAX_AGE_DAYS environment variable also sets the age.
"""

import argparse
import datetime
import json
import os
import pathlib
import sys

DEFAULT_MAX_AGE_DAYS = 180


def dated(node, path):
    """Yields (where, retrieved_at) for every object with retrieved_at."""
    if isinstance(node, dict):
        if "retrieved_at" in node:
            yield path, node["retrieved_at"]
        for key, value in node.items():
            yield from dated(value, f"{path}.{key}")
    elif isinstance(node, list):
        for index, value in enumerate(node):
            yield from dated(value, f"{path}[{index}]")


def main(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("dir", nargs="?", default=str(pathlib.Path(__file__).resolve().parent.parent / "model-cards"))
    parser.add_argument("--max-age-days", type=int, default=int(os.environ.get("MODEL_CARD_MAX_AGE_DAYS", DEFAULT_MAX_AGE_DAYS)))
    parser.add_argument("--today", type=datetime.date.fromisoformat, default=datetime.datetime.now(datetime.timezone.utc).date())
    args = parser.parse_args(argv)
    if args.max_age_days < 1:
        parser.error("--max-age-days must be at least 1")
    oldest = args.today - datetime.timedelta(days=args.max_age_days)
    problems = []
    checked = 0
    for file in sorted(pathlib.Path(args.dir).rglob("*.json")):
        name = file.relative_to(args.dir)
        try:
            document = json.loads(file.read_text(encoding="utf-8"))
        except (OSError, ValueError) as error:
            problems.append(f"{name}: unreadable: {error}")
            continue
        for where, value in dated(document, "$"):
            checked += 1
            try:
                day = datetime.date.fromisoformat(value)
            except (TypeError, ValueError):
                problems.append(f"{name} {where}: retrieved_at {value!r} is not a date")
                continue
            if day > args.today:
                problems.append(f"{name} {where}: retrieved_at {day} is in the future")
            elif day < oldest:
                problems.append(f"{name} {where}: retrieved_at {day} is older than {args.max_age_days} days; re-read its source")
    for problem in problems:
        print(problem, file=sys.stderr)
    if problems:
        print(f"{len(problems)} of {checked} model card facts need a re-read (model-cards/README.md, Refreshing)", file=sys.stderr)
        return 1
    print(f"{checked} model card facts read within {args.max_age_days} days")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
