"""The validator must reject cases a correct client could not pass."""

from __future__ import annotations

import copy
import importlib.util
import json
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
SPEC = importlib.util.spec_from_file_location("conformance_validate", HERE / "validate.py")
assert SPEC and SPEC.loader
validate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(validate)

DOCUMENT = json.loads((validate.ROOT / "openapi" / "inference-v1.json").read_text())
ERRORS = json.loads((HERE / "errors.json").read_text())
SAME_KEY = validate.retry_policy(ERRORS)
STATUSES = {entry["code"]: entry["status"] for entry in ERRORS["codes"]}


def case(prefix: str) -> dict:
    (path,) = (HERE / "cases").glob(f"{prefix}-*.json")
    return json.loads(path.read_text())


class ValidatorRejects(unittest.TestCase):
    def assert_rejected(self, mutated: dict) -> None:
        with self.assertRaises(validate.CaseError):
            validate.validate_case(mutated, DOCUMENT, SAME_KEY, STATUSES)

    def test_every_case_is_valid_as_written(self) -> None:
        for path in sorted((HERE / "cases").glob("*.json")):
            validate.validate_case(json.loads(path.read_text()), DOCUMENT, SAME_KEY, STATUSES)

    def test_wrong_waits_and_attempts(self) -> None:
        mutated = case("17")
        mutated["expect"]["sleeps"] = [1]
        self.assert_rejected(mutated)
        mutated = case("01")
        mutated["expect"]["attempts"] = 2
        self.assert_rejected(mutated)

    def test_a_retry_left_without_its_response(self) -> None:
        mutated = case("17")
        mutated["responses"] = mutated["responses"][:1]
        mutated["expect"] = {"attempts": 1, "sleeps": [3], "error": {
            "kind": "problem", "status": 429, "code": "rate_limit_exceeded",
            "request_id": "req-429", "retry_after": 3}}
        self.assert_rejected(mutated)

    def test_an_invalid_request_body(self) -> None:
        mutated = case("01")
        mutated["call"]["body"]["temperature"] = 5
        mutated["expect_request"]["body"]["temperature"] = 5
        self.assert_rejected(mutated)

    def test_an_error_kind_that_does_not_match_the_response(self) -> None:
        for kind in ({"kind": "protocol"}, {"kind": "stream", "code": "x", "status": 502}, {"kind": "http", "status": 401}):
            mutated = case("14")
            mutated["expect"]["error"] = kind
            self.assert_rejected(mutated)

    def test_retry_after_must_match_the_header_both_ways(self) -> None:
        mutated = case("14")
        mutated["expect"]["error"]["retry_after"] = 7
        self.assert_rejected(mutated)
        mutated = case("21")
        del mutated["expect"]["error"]["retry_after"]
        self.assert_rejected(mutated)

    def test_a_known_code_with_the_wrong_status(self) -> None:
        mutated = case("14")
        mutated["responses"][0]["status"] = 429
        mutated["responses"][0]["json"]["status"] = 429
        mutated["expect"]["error"]["status"] = 429
        self.assert_rejected(mutated)

    def test_an_unknown_code_keeps_any_status(self) -> None:
        mutated = case("22")
        for target in (mutated["responses"][0], mutated["responses"][0]["json"], mutated["expect"]["error"]):
            target["status"] = 418
        validate.validate_case(mutated, DOCUMENT, SAME_KEY, STATUSES)

    def test_wrong_stream_events(self) -> None:
        mutated = case("07")
        mutated["expect"]["events"][0]["data"]["model"] = "other"
        self.assert_rejected(mutated)
        mutated = case("11")
        mutated["expect"]["error"] = {"kind": "protocol"}
        self.assert_rejected(mutated)


class SseParsing(unittest.TestCase):
    def test_an_event_without_its_blank_line_is_not_dispatched(self) -> None:
        self.assertEqual(validate.parse_sse("event: a\ndata: 1\n"), [])
        self.assertEqual(validate.parse_sse("event: a\ndata: 1"), [])
        self.assertEqual(validate.parse_sse("event: a\ndata: 1\n\n"), [("a", "1")])

    def test_line_endings_comments_and_multiline_data(self) -> None:
        text = ": hi\r\nevent: a\r\ndata: x\r\ndata: y\r\r\nevent: b\rdata: z\r\r"
        self.assertEqual(validate.parse_sse(text), [("a", "x\ny"), ("b", "z")])

    def test_completed_before_finished_is_rejected(self) -> None:
        text = (
            'event: response.started\ndata: {"model":"m"}\n\n'
            'event: choice.delta\ndata: {"choice_index":0,"content_delta":"a","refusal_delta":null,"annotations":[]}\n\n'
            'event: response.completed\ndata: {"model":"m","usage":null}\n\n'
        )
        with self.assertRaises(validate.CaseError):
            validate.validate_stream(text, DOCUMENT, "test")

    def test_a_stream_may_start_with_its_terminal_error(self) -> None:
        validate.validate_stream('event: error\ndata: {"code":"provider_failure","status":502}\n\n', DOCUMENT, "test")

    def test_a_stream_may_not_start_with_a_delta(self) -> None:
        text = 'event: choice.delta\ndata: {"choice_index":0,"content_delta":"a","refusal_delta":null,"annotations":[]}\n\n'
        with self.assertRaises(validate.CaseError):
            validate.validate_stream(text, DOCUMENT, "test")


class Taxonomy(unittest.TestCase):
    def test_same_key_codes_are_never_reserved(self) -> None:
        mutated = copy.deepcopy(ERRORS)
        for entry in mutated["codes"]:
            if entry["code"] == "rate_limit_exceeded":
                entry["reserved"] = True
        with self.assertRaises(validate.CaseError):
            validate.validate_errors(mutated, DOCUMENT)


if __name__ == "__main__":
    unittest.main()
