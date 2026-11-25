"""Unit tests for release.py (the version scheme, check, notes and bump).

    python3 -m unittest discover -s clients/release -p 'test_*.py'

bump and check run on a copy of this repository's declaration and changelog
files in a temporary directory, so they exercise the real patterns without
touching the checkout.
"""
import datetime
import io
import json
import shutil
import sys
import tempfile
import unittest
from contextlib import redirect_stderr, redirect_stdout
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release  # noqa: E402
import version as client_version  # noqa: E402
from release import scheme  # noqa: E402


class SchemeTest(unittest.TestCase):
    def test_parses_year_month_and_patch(self):
        v = scheme("2026.11.0")
        self.assertEqual((v.year, v.month, v.patch, v.rc, v.prerelease), (2026, 11, 0, None, False))
        self.assertEqual((scheme("2027.1.2").month, scheme("2027.1.2").patch), (1, 2))
        self.assertEqual(scheme("2026.12.31").patch, 31)

    def test_pre_release(self):
        v = scheme("2026.11.1-rc.2")
        self.assertEqual((v.patch, v.rc, v.prerelease), (1, 2, True))

    def test_rejects_leading_zeros(self):
        for text in ["2026.01.0", "2026.011.0", "2026.11.00", "2026.11.01", "02026.11.0", "2026.11.0-rc.01"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                scheme(text)

    def test_rejects_months_that_do_not_exist(self):
        for text in ["2026.0.0", "2026.13.0", "2026.99.0", "2026.100.0"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                scheme(text)

    def test_rejects_malformed_versions(self):
        for text in ["2026.11", "2026.11.0.1", "v2026.11.0", "2026.11.0-beta.1", "2026.11.0-rc",
                     "0226.11.0", "26.11.0", " 2026.11.0", "2026.11.0\n", "2026.1125.0"]:
            with self.subTest(text=text), self.assertRaises(ValueError):
                scheme(text)

    def test_ordering(self):
        ordered = ["2026.9.0", "2026.10.0", "2026.11.0-rc.1", "2026.11.0-rc.2", "2026.11.0", "2026.11.1",
                   "2026.12.3", "2027.1.0"]
        keys = [scheme(text).sort_key() for text in ordered]
        self.assertEqual(keys, sorted(keys))
        self.assertEqual(len(set(keys)), len(keys))

    def test_ordering_is_semver_numeric(self):
        # The three numbers compare as integers, so semver tools (cargo, npm,
        # Maven) order releases as months do: 2026.9 before 2026.10.
        self.assertLess(scheme("2026.9.0").numbers, scheme("2026.10.0").numbers)
        self.assertLess(scheme("2026.12.9").numbers, scheme("2027.1.0").numbers)

    def test_go_module_version(self):
        cases = {"2026.11.0": "v0.202611.0", "2027.1.2": "v0.202701.2", "2026.9.0": "v0.202609.0",
                 "2026.12.10": "v0.202612.10", "2026.11.1-rc.1": "v0.202611.1-rc.1"}
        for text, go in cases.items():
            with self.subTest(text=text):
                self.assertEqual(scheme(text).go_module_version(), go)

    def test_go_module_versions_order_as_releases(self):
        # The two-digit month keeps v0.YYYYMM numerically ordered.
        texts = ["2026.9.0", "2026.10.0", "2026.12.3", "2027.1.0", "2027.1.1"]
        go = [tuple(int(n) for n in scheme(t).go_module_version()[1:].split(".")) for t in texts]
        self.assertEqual(go, sorted(go))

    def test_image_tags(self):
        self.assertEqual(scheme("2026.11.0").image_tags(), ["2026.11.0", "2026.11", "latest"])
        self.assertEqual(scheme("2026.11.1-rc.1").image_tags(), ["2026.11.1-rc.1"])

    def test_python_version(self):
        self.assertEqual(scheme("2026.11.0").python_version(), "2026.11.0")
        self.assertEqual(scheme("2026.11.0-rc.3").python_version(), "2026.11.0rc3")

    def test_for_date(self):
        self.assertEqual(release.for_date(datetime.date(2027, 1, 5)), "2027.1.0")
        self.assertEqual(release.for_date(datetime.date(2026, 11, 25), 2), "2026.11.2")

    def test_versions_are_client_semver(self):
        for text in ["2026.11.0", "2026.11.0-rc.1"]:
            self.assertRegex(text, client_version.SEMVER)

    def test_tags(self):
        self.assertEqual(release.parse_tag("v2026.11.0").text, "2026.11.0")
        for tag in ["2026.11.0", "client-python-v0.2.0", "v1.2.3", "v2026.01.0"]:
            with self.subTest(tag=tag), self.assertRaises(ValueError):
                release.parse_tag(tag)


class TreeTest(unittest.TestCase):
    """bump, check and notes on a copy of the repository's version files."""

    def setUp(self):
        self.root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.root)
        files = {d.file for d in release.DECLARATIONS} | set(release.CHANGELOGS.values())
        for file in files:
            (self.root / file).parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(release.ROOT / file, self.root / file)
        release.bump(scheme("2030.1.0"), self.root)
        for component, file in release.CHANGELOGS.items():
            heading = "## [2030.1.0] - 2030-01-15" if component == "gateway" else "## 2030.1.0"
            (self.root / file).write_text(f"# Changelog\n\n{heading}\n\n### Added\n\n- {component} notes.\n\n"
                                          "## 1.0.0\n\n- Old.\n", encoding="utf-8")

    def read(self, file):
        return (self.root / file).read_text(encoding="utf-8")

    def test_bump_rewrites_every_declaration(self):
        changed = release.bump(scheme("2031.12.4-rc.1"), self.root)
        self.assertEqual(set(changed), {d.file for d in release.DECLARATIONS})
        self.assertEqual(release.current(self.root), "2031.12.4-rc.1")
        header = self.read("clients/c/include/gateway/gateway.h")
        self.assertIn("#define GW_VERSION_MAJOR 2031\n", header)
        self.assertIn("#define GW_VERSION_MINOR 12\n", header)
        self.assertIn("#define GW_VERSION_PATCH 4\n", header)
        self.assertIn('#define GW_VERSION_STRING "2031.12.4-rc.1"', header)
        for lock in ["apps/admin/package-lock.json", "apps/docs/package-lock.json"]:
            data = json.loads(self.read(lock))
            self.assertEqual((data["version"], data["packages"][""]["version"]),
                             ("2031.12.4-rc.1", "2031.12.4-rc.1"))
        self.assertIn('name = "gateway"\nversion = "2031.12.4-rc.1"', self.read("Cargo.lock"))
        snippets = self.read("apps/admin/src/client-versions.ts")
        self.assertIn("CLIENT_VERSION = '2031.12.4-rc.1';", snippets)
        self.assertIn("GO_MODULE_VERSION = 'v0.203112.4-rc.1';", snippets)
        self.assertIn("clients/go@v0.203112.4-rc.1", self.read("apps/docs/clients/index.md"))
        self.assertIn('name = "qucelerate-ai-gateway"\nversion = "2031.12.4-rc.1"',
                      self.read("clients/rust/Cargo.lock"))

    def test_bump_leaves_dependency_versions_alone(self):
        before = json.loads(self.read("apps/admin/package-lock.json"))
        release.bump(scheme("2031.1.0"), self.root)
        after = json.loads(self.read("apps/admin/package-lock.json"))
        for name, entry in before["packages"].items():
            if name:
                self.assertEqual(entry.get("version"), after["packages"][name].get("version"), name)

    def test_bump_twice_changes_nothing(self):
        self.assertEqual(release.bump(scheme("2030.1.0"), self.root), [])

    def test_check_passes_a_consistent_tree(self):
        self.assertEqual(release.problems(scheme("2030.1.0"), self.root), [])

    def test_check_names_a_disagreeing_declaration(self):
        path = self.root / "deploy/helm/ai-gateway/Chart.yaml"
        path.write_text(path.read_text().replace('appVersion: "2030.1.0"', 'appVersion: "2030.1.1"'))
        found = release.problems(scheme("2030.1.0"), self.root)
        self.assertEqual(len(found), 1)
        self.assertIn("Chart.yaml: declares 2030.1.1", found[0])
        with self.assertRaises(ValueError):
            release.current(self.root)

    def test_check_names_missing_notes(self):
        (self.root / "clients/go/CHANGELOG.md").write_text("# Changelog\n\n## 2030.1.0\n\n## 1.0.0\n\n- x\n")
        found = release.problems(scheme("2030.1.0"), self.root)
        self.assertEqual(found, ["clients/go/CHANGELOG.md has no notes for 2030.1.0 (a '## 2030.1.0' section)"])

    def test_check_names_a_changelog_date_outside_the_month(self):
        path = self.root / "CHANGELOG.md"
        path.write_text(path.read_text().replace("2030-01-15", "2030-02-01"))
        found = release.problems(scheme("2030.1.0"), self.root)
        self.assertEqual(len(found), 1)
        self.assertIn("dates 2030.1.0 2030-02-01", found[0])

    def test_check_wants_every_version_equal(self):
        found = release.problems(scheme("2030.1.1"), self.root)
        self.assertTrue(found)
        self.assertTrue(all("2030.1.1" in line or "v0.203001.1" in line for line in found))

    def test_notes_hold_every_component(self):
        text = release.notes(scheme("2030.1.0"), self.root)
        for title in release.CHANGELOG_TITLES.values():
            self.assertIn(f"## {title}\n", text)
        self.assertIn("### Added", text)
        self.assertNotIn("Old.", text)

    def test_section_ignores_link_definitions(self):
        text = "## [1.0.0] - 2030-01-01\n\n[1.0.0]: https://example.com/v1.0.0\n"
        self.assertIsNone(release.section(text, "1.0.0"))
        self.assertEqual(release.section("## [1.0.0]\n\n- x\n\n[1.0.0]: https://e/\n", "1.0.0"), "- x")

    def test_move_notes_renames_the_unreleased_sections(self):
        path = self.root / "CHANGELOG.md"
        path.write_text(path.read_text() + "\n[2030.1.0]: https://example.com/releases/tag/v2030.1.0\n")
        release.bump(scheme("2030.2.0"), self.root, move_notes=True, today=datetime.date(2030, 2, 3))
        self.assertEqual(release.problems(scheme("2030.2.0"), self.root), [])
        root_notes = self.read("CHANGELOG.md")
        self.assertIn("## [2030.2.0] - 2030-02-03", root_notes)
        self.assertIn("[2030.2.0]: https://example.com/releases/tag/v2030.2.0", root_notes)
        self.assertIn("## 2030.2.0\n", self.read("clients/python/CHANGELOG.md"))
        self.assertIn("## 1.0.0", self.read("clients/python/CHANGELOG.md"))

    def test_client_version_script_accepts_a_pre_release(self):
        # version.py checks the C header's numeric macros against the
        # version without its pre-release suffix.
        release.bump(scheme("2030.1.0-rc.1"), self.root)
        saved = client_version.CLIENTS
        client_version.CLIENTS = self.root / "clients"
        try:
            self.assertEqual(client_version.version("c"), "2030.1.0-rc.1")
        finally:
            client_version.CLIENTS = saved


class CommandLineTest(unittest.TestCase):
    def run_main(self, *argv):
        out, err = io.StringIO(), io.StringIO()
        with redirect_stdout(out), redirect_stderr(err):
            code = release.main(list(argv))
        return code, out.getvalue(), err.getvalue()

    def test_usage(self):
        self.assertEqual(self.run_main("frobnicate")[0], 2)

    def test_check_rejects_a_bad_tag(self):
        code, _, err = self.run_main("check", "v2026.13.0")
        self.assertEqual(code, 1)
        self.assertIn("not a month", err)

    def test_check_outputs_for_this_checkout(self):
        current = release.current()
        code, out, err = self.run_main("check", f"v{current}")
        self.assertEqual(code, 0, err)
        values = dict(line.split("=", 1) for line in out.splitlines())
        self.assertEqual(values["version"], current)
        self.assertEqual(values["go_module_tag"], f"clients/go/{scheme(current).go_module_version()}")
        self.assertEqual(values["image_tags"].split(), scheme(current).image_tags())


if __name__ == "__main__":
    unittest.main()
