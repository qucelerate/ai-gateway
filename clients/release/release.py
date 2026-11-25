#!/usr/bin/env python3
"""One version for every component of a release: check it, derive from it, bump it.

    clients/release/release.py check v<version>   # a release tag: print GitHub outputs
    clients/release/release.py current            # the version every declaration agrees on
    clients/release/release.py notes <version>    # the GitHub release notes
    clients/release/release.py bump [<version>] [--move-notes]

A release is one tag, v<version>, and every component carries that version:
the gateway (Cargo.toml and Cargo.lock), the admin console and the docs site
(package.json and package-lock.json), the Helm chart (version and
appVersion), and the Rust, Python, Go, Java and C clients (every declaration
version.py reads, plus the Rust client's Cargo.lock).

Versions are calendar versions, YYYY.M.PATCH: the year, the month without a
leading zero (2026.11.0, 2027.1.0), and PATCH, which counts the releases made
in that month from 0. A pre-release appends -rc.N. `scheme()` below is the one
place that knows this; everything else derives from what it returns.

`check` validates a tag, checks every declaration equals its version and
that CHANGELOG.md and each clients/*/CHANGELOG.md have notes for it, and
prints `key=value` lines for $GITHUB_OUTPUT: version, prerelease, month,
image_tags (X.Y.Z, and for a full release also YYYY.M and latest),
go_module_tag (clients/go/v0.YYYYMM.PATCH, the month two digits: a Go module
cannot have a major version of 2 or more without a path suffix) and
python_version (the PEP 440 spelling of the version, as PyPI and the wheel's
file name show it).

`bump` rewrites every declaration (default: this month's first version,
YYYY.M.0, in UTC). With --move-notes it also renames the changelog sections
of the version it replaces (the root changelog's date becomes today), for a
release whose version moved before it was tagged; never use it on a version
that was released.
"""
from __future__ import annotations

import datetime
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Optional

sys.path.insert(0, str(Path(__file__).resolve().parent))
from version import DECLARATIONS as CLIENT_DECLARATIONS  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent.parent

CLIENTS = tuple(CLIENT_DECLARATIONS)


# --- The version scheme ---------------------------------------------------

@dataclass(frozen=True)
class Version:
    """A parsed release version. `text` is it as written."""

    text: str
    year: int
    month: int
    patch: int
    rc: Optional[int]

    @property
    def prerelease(self) -> bool:
        return self.rc is not None

    @property
    def numbers(self) -> tuple[int, int, int]:
        """The three dot-separated numbers (the C header's MAJOR, MINOR, PATCH)."""
        return (self.year, self.month, self.patch)

    @property
    def month_text(self) -> str:
        """The release's month as YYYY-MM."""
        return f"{self.year:04d}-{self.month:02d}"

    @property
    def release_line(self) -> str:
        """The floating image tag of the month: YYYY.M."""
        return "{}.{}".format(*self.numbers[:2])

    def sort_key(self) -> tuple[int, int, int, int, int]:
        # A pre-release sorts before its release.
        return (*self.numbers, 0 if self.prerelease else 1, self.rc or 0)

    def image_tags(self) -> list[str]:
        if self.prerelease:
            return [self.text]
        return [self.text, self.release_line, "latest"]

    def go_module_version(self) -> str:
        suffix = f"-rc.{self.rc}" if self.prerelease else ""
        return f"v0.{self.year:04d}{self.month:02d}.{self.patch}{suffix}"

    def python_version(self) -> str:
        suffix = f"rc{self.rc}" if self.prerelease else ""
        return "{}.{}.{}".format(*self.numbers) + suffix


SCHEME = re.compile(
    r"(?P<year>[1-9][0-9]{3})\.(?P<month>[1-9][0-9]?)\.(?P<patch>0|[1-9][0-9]*)"
    r"(?:-rc\.(?P<rc>0|[1-9][0-9]*))?"
)


def scheme(text: str) -> Version:
    """Parse a version; ValueError if it is not YYYY.M.PATCH[-rc.N] with a month 1-12."""
    match = SCHEME.fullmatch(text)
    if not match:
        raise ValueError(f"{text!r} is not a release version (YYYY.M.PATCH or YYYY.M.PATCH-rc.N,"
                         " no leading zeros)")
    month = int(match["month"])
    if not 1 <= month <= 12:
        raise ValueError(f"{text!r}: {month} is not a month")
    return Version(text, int(match["year"]), month, int(match["patch"]),
                   None if match["rc"] is None else int(match["rc"]))


def for_date(date: datetime.date, patch: int = 0) -> str:
    """The version of the `patch`th release in `date`'s month."""
    return f"{date.year}.{date.month}.{patch}"


def parse_tag(tag: str) -> Version:
    if not tag.startswith("v"):
        raise ValueError(f"{tag!r} is not a release tag (v<version>)")
    return scheme(tag[1:])


# --- Where the version is declared ----------------------------------------

@dataclass(frozen=True)
class Declaration:
    component: str
    file: str
    pattern: str
    # "version": the version as written; "go": the Go module's version
    # derived from it. A pattern with three groups states the three numbers
    # alone (the C header's MAJOR, MINOR and PATCH macros).
    kind: str = "version"

    @property
    def partial(self) -> bool:
        """It states something derived from the version, not the version itself."""
        return self.kind != "version" or re.compile(self.pattern).groups > 1

    def expects(self, version: Version) -> str:
        if self.kind == "go":
            return version.go_module_version()
        if re.compile(self.pattern).groups > 1:
            return "{}.{}.{}".format(*version.numbers)
        return version.text


# package.json and package-lock.json: the top-level "version" (two-space
# indent, npm's format), and the lockfile's own entry for the root package.
TOP_LEVEL_VERSION = r'(?m)^  "version": "([^"]+)"'
LOCKED_ROOT_VERSION = r'"packages": \{\s*"": \{[^{}]*?"version": "([^"]+)"'


def _declarations() -> list[Declaration]:
    found = [
        Declaration("gateway", "Cargo.toml", r'(?m)^version = "([^"]+)"'),
        Declaration("gateway", "Cargo.lock", r'(?m)^name = "gateway"\nversion = "([^"]+)"'),
        Declaration("admin", "apps/admin/package.json", TOP_LEVEL_VERSION),
        Declaration("admin", "apps/admin/package-lock.json", TOP_LEVEL_VERSION),
        Declaration("admin", "apps/admin/package-lock.json", LOCKED_ROOT_VERSION),
        # The versions the console's code snippets install.
        Declaration("admin", "apps/admin/src/client-versions.ts", r"(?m)^export const CLIENT_VERSION = '([^']+)';"),
        Declaration("admin", "apps/admin/src/client-versions.ts",
                    r"(?m)^export const GO_MODULE_VERSION = '([^']+)';", kind="go"),
        Declaration("docs", "apps/docs/package.json", TOP_LEVEL_VERSION),
        Declaration("docs", "apps/docs/package-lock.json", TOP_LEVEL_VERSION),
        Declaration("docs", "apps/docs/package-lock.json", LOCKED_ROOT_VERSION),
        Declaration("chart", "deploy/helm/ai-gateway/Chart.yaml", r"(?m)^version: (\S+)$"),
        Declaration("chart", "deploy/helm/ai-gateway/Chart.yaml", r'(?m)^appVersion: "([^"]+)"$'),
    ]
    for client, declarations in CLIENT_DECLARATIONS.items():
        found += [Declaration(client, f"clients/{file}", pattern) for file, pattern in declarations]
    found.append(Declaration("rust", "clients/rust/Cargo.lock",
                             r'(?m)^name = "qucelerate-ai-gateway"\nversion = "([^"]+)"'))
    # The install instructions in the documentation and the client READMEs.
    clients_page = "apps/docs/clients/index.md"
    found += [
        Declaration("docs", clients_page, r"pip install qucelerate-ai-gateway==(\S+)"),
        Declaration("docs", clients_page, r"cargo add qucelerate-ai-gateway@=(\S+)"),
        Declaration("docs", clients_page, r"go get github\.com/qucelerate/ai-gateway/clients/go@(\S+)", kind="go"),
        Declaration("docs", clients_page, r"the dependency `com\.qucelerate:qucelerate-ai-gateway:([^`]+)`"),
        Declaration("docs", clients_page, r"<artifactId>qucelerate-ai-gateway</artifactId>\s*<version>([^<]+)</version>"),
        Declaration("docs", clients_page, r'pin the exact version \(`qucelerate-ai-gateway = "=([^"]+)"`\)'),
        Declaration("rust", "clients/rust/README.md", r"cargo add qucelerate-ai-gateway@=(\S+)"),
        Declaration("go", "clients/go/README.md", r"go get github\.com/qucelerate/ai-gateway/clients/go@(\S+)", kind="go"),
        Declaration("java", "clients/java/README.md",
                    r"<artifactId>qucelerate-ai-gateway</artifactId>\s*<version>([^<]+)</version>"),
    ]
    return found


DECLARATIONS = _declarations()

# Each component's release notes: the root changelog's section is headed
# `## [<version>] - YYYY-MM-DD`, a client's `## <version>`.
CHANGELOGS = {"gateway": "CHANGELOG.md", **{client: f"clients/{client}/CHANGELOG.md" for client in CLIENTS}}
CHANGELOG_TITLES = {"gateway": "Gateway, admin console, chart and documentation",
                    "rust": "Rust client", "python": "Python client", "go": "Go client",
                    "java": "Java client", "c": "C and C++ client"}


def _value(match: re.Match) -> str:
    return ".".join(match.groups())


def declared(root: Path = ROOT) -> list[tuple[Declaration, Optional[str]]]:
    """Every declaration with the version it states (None when its pattern finds nothing)."""
    result = []
    for declaration in DECLARATIONS:
        path = root / declaration.file
        text = path.read_text(encoding="utf-8") if path.is_file() else ""
        match = re.search(declaration.pattern, text)
        result.append((declaration, _value(match) if match else None))
    return result


def section(text: str, version: str) -> Optional[str]:
    """The body of a changelog's section for `version`, or None when missing or empty."""
    heading = re.compile(
        rf"^## (?:{re.escape(version)}|\[{re.escape(version)}\])(?: - \d{{4}}-\d{{2}}-\d{{2}})?[ \t]*$", re.M)
    match = heading.search(text)
    if not match:
        return None
    rest = text[match.end():]
    end = re.search(r"^## ", rest, re.M)
    body = (rest[: end.start()] if end else rest)
    # Link reference definitions at the end of the file are not notes.
    body = re.sub(r"(?m)^\[[^\]]+\]: \S+\s*$", "", body).strip()
    return body or None


def problems(version: Version, root: Path = ROOT) -> list[str]:
    """Everything that stops `version` from being released from `root`."""
    found = []
    for declaration, value in declared(root):
        if value is None:
            found.append(f"{declaration.file}: no version declaration matching {declaration.pattern!r}")
        elif value != declaration.expects(version):
            found.append(f"{declaration.file}: declares {value}, not {declaration.expects(version)}"
                         f" ({declaration.component})")
    for component, file in CHANGELOGS.items():
        path = root / file
        if not path.is_file():
            found.append(f"{file} is missing")
        elif section(path.read_text(encoding="utf-8"), version.text) is None:
            found.append(f"{file} has no notes for {version.text} (a '## {version.text}' section)")
    root_notes = (root / CHANGELOGS["gateway"])
    if root_notes.is_file():
        dated = re.search(rf"^## \[{re.escape(version.text)}\] - (\d{{4}}-\d{{2}}-\d{{2}})",
                          root_notes.read_text(encoding="utf-8"), re.M)
        if dated and dated[1][:7] != version.month_text:
            found.append(f"CHANGELOG.md dates {version.text} {dated[1]}, outside its month ({version.month_text})")
    return found


def outputs(version: Version) -> dict[str, str]:
    return {
        "version": version.text,
        "prerelease": "true" if version.prerelease else "false",
        "month": version.month_text,
        "image_tags": " ".join(version.image_tags()),
        "go_module_tag": f"clients/go/{version.go_module_version()}",
        "python_version": version.python_version(),
    }


def notes(version: Version, root: Path = ROOT) -> str:
    parts = []
    for component, file in CHANGELOGS.items():
        body = section((root / file).read_text(encoding="utf-8"), version.text)
        if body is None:
            raise ValueError(f"{file} has no notes for {version.text}")
        # A section's own headings (### Added, ### Breaking) sit below the
        # component's `##` title as they are.
        parts.append(f"## {CHANGELOG_TITLES[component]}\n\n{body}")
    return "\n\n".join(parts) + "\n"


def bump(new: Version, root: Path = ROOT, move_notes: bool = False,
         today: Optional[datetime.date] = None) -> list[str]:
    """Rewrite every declaration to `new`; return the files changed.

    With `move_notes`, the changelog sections of the version replaced are
    renamed to `new`, the root changelog's dated `today` (default: today, UTC).
    """
    texts: dict[str, str] = {}
    old_versions = set()
    for declaration in DECLARATIONS:
        text = texts.get(declaration.file)
        if text is None:
            text = (root / declaration.file).read_text(encoding="utf-8")
        match = re.search(declaration.pattern, text)
        if not match:
            raise ValueError(f"{declaration.file}: no version declaration matching {declaration.pattern!r}")
        if not declaration.partial:
            old_versions.add(_value(match))
        values = declaration.expects(new).split(".") if match.re.groups > 1 else [declaration.expects(new)]
        if len(values) != match.re.groups:
            raise ValueError(f"{declaration.file}: {match.re.groups} groups, cannot write {new.text}")
        for index in range(match.re.groups, 0, -1):  # right to left: earlier spans stay valid
            start, end = match.span(index)
            text = text[:start] + values[index - 1] + text[end:]
        texts[declaration.file] = text
    if move_notes:
        if len(old_versions) != 1:
            raise ValueError("--move-notes needs every declaration to agree first: "
                             + ", ".join(sorted(old_versions)))
        (old,) = old_versions
        dated = today or datetime.datetime.now(datetime.timezone.utc).date()
        for file in CHANGELOGS.values():
            texts[file] = _move_notes(
                texts.get(file) or (root / file).read_text(encoding="utf-8"), old, new, dated)
    changed = []
    for file, text in texts.items():
        path = root / file
        if path.read_text(encoding="utf-8") != text:
            path.write_text(text, encoding="utf-8")
            changed.append(file)
    return changed


def _move_notes(text: str, old: str, new: Version, dated: datetime.date) -> str:
    escaped = re.escape(old)
    text = re.sub(rf"(?m)^## \[{escaped}\](?: - \d{{4}}-\d{{2}}-\d{{2}})?",
                  f"## [{new.text}] - {dated.isoformat()}", text)
    text = re.sub(rf"(?m)^## {escaped}(?= - |\s*$)", f"## {new.text}", text)
    text = re.sub(rf"(?m)^\[{escaped}\]: (\S*?){escaped}\s*$",
                  lambda m: f"[{new.text}]: {m[1]}{new.text}", text)
    return text


def current(root: Path = ROOT) -> str:
    """The version every declaration states; ValueError when they disagree."""
    found = declared(root)
    values = {value for declaration, value in found if not declaration.partial}
    if len(values) == 1 and None not in values:
        (value,) = values
        version = scheme(value)
        if all(value == declaration.expects(version) for declaration, value in found):
            return value
    raise ValueError("the declarations disagree:\n" + "\n".join(f"  {d.file}: {v}" for d, v in found))


def _utc_today() -> datetime.date:
    return datetime.datetime.now(datetime.timezone.utc).date()


def main(argv: list[str], today: Callable[[], datetime.date] = _utc_today) -> int:
    usage = __doc__.split("\n\n")[1]
    try:
        if len(argv) == 2 and argv[0] == "check":
            version = parse_tag(argv[1])
            found = problems(version)
            if found:
                print(f"{argv[1]} cannot be released:", *found, sep="\n  ", file=sys.stderr)
                return 1
            for key, value in outputs(version).items():
                print(f"{key}={value}")
        elif argv == ["current"]:
            version = scheme(current())
            found = problems(version)
            if found:
                print(f"{version.text}:", *found, sep="\n  ", file=sys.stderr)
                return 1
            print(version.text)
        elif len(argv) == 2 and argv[0] == "notes":
            print(notes(scheme(argv[1])), end="")
        elif argv and argv[0] == "bump" and len(argv) <= 3:
            rest = argv[1:]
            move = "--move-notes" in rest
            rest = [a for a in rest if a != "--move-notes"]
            if len(rest) > 1:
                raise ValueError("bump takes one version")
            new = scheme(rest[0] if rest else for_date(today()))
            for file in bump(new, move_notes=move, today=today()):
                print(f"updated {file}")
        else:
            print(usage, file=sys.stderr)
            return 2
    except (ValueError, OSError) as error:
        print(error, file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
