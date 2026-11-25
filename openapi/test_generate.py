import copy
import importlib.util
import json
import tempfile
import unittest
from pathlib import Path


SPEC = importlib.util.spec_from_file_location("openapi_generate", Path(__file__).with_name("generate.py"))
assert SPEC and SPEC.loader
generate = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(generate)

SSE_SPEC = importlib.util.spec_from_file_location(
    "openapi_sse_as_operations", Path(__file__).with_name("sse_as_operations.py")
)
assert SSE_SPEC and SSE_SPEC.loader
sse_as_operations = importlib.util.module_from_spec(SSE_SPEC)
SSE_SPEC.loader.exec_module(sse_as_operations)


class OpenApiContractTests(unittest.TestCase):
    def test_embedding_input_items_require_nonempty_strings(self):
        document = json.loads(generate.SOURCE.read_text())
        item_schema = document["components"]["schemas"]["EmbeddingRequest"][
            "properties"
        ]["input"]["items"]
        self.assertEqual(item_schema["type"], "string")
        self.assertEqual(item_schema["minLength"], 1)


class SseContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.document = json.loads(generate.SOURCE.read_text())
        cls.operation = cls.document["paths"][
            "/api/inference/v1/chat/completions/stream"
        ]["post"]
        cls.fixture = generate.FIXTURE.read_text()

    def assert_rejected(self, fixture):
        with self.assertRaises(AssertionError):
            generate.validate_sse_stream(fixture, self.operation, self.document)

    def assert_rejected_with(self, fixture, operation):
        with self.assertRaises(AssertionError):
            generate.validate_sse_stream(fixture, operation, self.document)

    def test_malformed_record_framing_is_rejected(self):
        malformed = self.fixture.replace("\n\nevent: choice.finished", "\nevent: choice.finished", 1)
        self.assert_rejected(malformed)

    def test_stream_must_begin_with_response_started(self):
        records = self.fixture.rstrip("\n").split("\n\n")
        malformed = "\n\n".join([records[1], records[3]]) + "\n\n"
        self.assert_rejected(malformed)

    def test_stream_may_begin_with_terminal_error(self):
        fixture = "event: error\ndata: {\"code\":\"provider_error\",\"status\":502}\n\n"
        generate.validate_sse_stream(fixture, self.operation, self.document)

    def test_nonterminal_error_cannot_be_first(self):
        operation = copy.deepcopy(self.operation)
        error_event = next(
            event for event in operation["x-sse-events"] if event["name"] == "error"
        )
        error_event["terminal"] = False
        fixture = "event: error\ndata: {\"code\":\"provider_error\",\"status\":502}\n\n"
        self.assert_rejected_with(fixture, operation)

    def test_error_first_stream_cannot_have_following_record(self):
        fixture = (
            "event: error\n"
            "data: {\"code\":\"provider_error\",\"status\":502}\n\n"
            "event: response.started\n"
            "data: {\"model\":\"chat-default\"}\n\n"
        )
        self.assert_rejected(fixture)

    def test_error_first_stream_must_match_error_schema(self):
        fixture = "event: error\ndata: {\"code\":\"provider_error\",\"status\":\"502\"}\n\n"
        self.assert_rejected(fixture)

    def test_usage_before_choice_finished_is_valid(self):
        generate.validate_sse_stream(self.fixture, self.operation, self.document)

    def test_response_completed_requires_finish_for_each_observed_choice(self):
        fixture = self.fixture.replace(
            'event: choice.finished\ndata: {"choice_index":0,"finish_reason":"stop"}\n\n',
            "",
            1,
        )
        self.assert_rejected(fixture)

    def test_response_completed_after_observed_choice_finished_is_valid(self):
        generate.validate_sse_stream(self.fixture, self.operation, self.document)

    def test_delta_events_may_be_interleaved(self):
        generate.validate_sse_stream(self.fixture, self.operation, self.document)

    def test_decreasing_interim_usage_is_rejected(self):
        fixture = self.fixture.replace(
            "event: choice.finished\n",
            'event: usage\ndata: {"output_tokens":0}\n\n'
            "event: choice.finished\n",
            1,
        )
        self.assert_rejected(fixture)

    def test_dropped_known_interim_usage_is_rejected(self):
        for usage in (
            '{"input_tokens":1}',
            '{"input_tokens":1,"output_tokens":null}',
        ):
            with self.subTest(usage=usage):
                fixture = self.fixture.replace(
                    "event: choice.finished\n",
                    f"event: usage\ndata: {usage}\n\n"
                    "event: choice.finished\n",
                    1,
                )
                self.assert_rejected(fixture)

    def test_completion_usage_regression_is_rejected(self):
        fixture = self.fixture.replace(
            'data: {"model":"chat-default","usage":null}',
            'data: {"model":"chat-default","usage":{"output_tokens":0}}',
            1,
        )
        self.assert_rejected(fixture)

    def test_cumulative_usage_update_is_valid(self):
        fixture = self.fixture.replace(
            "event: choice.finished\n",
            'event: usage\ndata: {"input_tokens":1,"output_tokens":2}\n\n'
            "event: choice.finished\n",
            1,
        )
        generate.validate_sse_stream(fixture, self.operation, self.document)

    def test_delta_after_choice_finished_is_rejected(self):
        fixture = self.fixture.replace(
            "event: response.completed\n",
            'event: choice.delta\ndata: {"choice_index":0,"content_delta":"late","refusal_delta":null,"annotations":[]}\n\n'
            "event: response.completed\n",
            1,
        )
        self.assert_rejected(fixture)

    def test_duplicate_choice_finished_is_rejected(self):
        fixture = self.fixture.replace(
            "event: response.completed\n",
            'event: choice.finished\ndata: {"choice_index":0,"finish_reason":"stop"}\n\n'
            "event: response.completed\n",
            1,
        )
        self.assert_rejected(fixture)

    def test_duplicate_response_started_is_rejected(self):
        duplicate = self.fixture.replace(
            "event: choice.finished\n",
            'event: response.started\ndata: {"model":"chat-default"}\n\n'
            "event: choice.finished\n",
            1,
        )
        self.assert_rejected(duplicate)

    def test_undeclared_event_and_out_of_order_events_are_rejected(self):
        self.assert_rejected(self.fixture.replace("event: usage", "event: not-declared", 1))

        records = self.fixture.rstrip("\n").split("\n\n")
        out_of_order = "\n\n".join([records[1], records[0], records[3]]) + "\n\n"
        self.assert_rejected(out_of_order)

    def test_schema_invalid_payload_is_rejected(self):
        invalid = self.fixture.replace(
            'data: {"model":"chat-default","usage":null}',
            'data: {"usage":null}',
            1,
        )
        self.assert_rejected(invalid)

    def test_x_fixture_is_linked_and_validated(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / "fixtures" / "sse" / "chat-completed.sse"
            fixture.parent.mkdir(parents=True)
            fixture.write_text(self.fixture)
            error_fixture = root / "fixtures" / "sse" / "chat-error.sse"
            error_fixture.write_text(
                "event: error\n"
                "data: {\"code\":\"provider_error\",\"status\":502}\n\n"
            )
            generate.validate(copy.deepcopy(self.document), root=root)

            document = copy.deepcopy(self.document)
            example = document["paths"][
                "/api/inference/v1/chat/completions/stream"
            ]["post"]["responses"]["200"]["content"]["text/event-stream"]["examples"]["completed"]
            example["x-fixture"] = "fixtures/sse/missing.sse"
            with self.assertRaises(AssertionError):
                generate.validate(document, root=root)


class StableContractTests(unittest.TestCase):
    """v1 is stable (1.0.0): changes against the frozen baseline must be compatible.

    oasdiff checks paths and schemas in CI, over copies in which
    sse_as_operations.py exposes each SSE event payload as an operation;
    these cover the rest.
    """

    STREAM = "/api/inference/v1/chat/completions/stream"
    REQUESTS = {"ChatRequest", "Message", "EmbeddingRequest", "Tool", "ToolCallRequest"}

    @classmethod
    def setUpClass(cls):
        cls.baseline = json.loads(
            (generate.ROOT / "baselines" / "inference-v1.0.0.json").read_text()
        )
        cls.current = json.loads(generate.TARGET.read_text())

    def test_major_version_stays_one(self):
        self.assertTrue(self.current["info"]["version"].startswith("1."))

    def test_baseline_sse_events_are_kept_with_their_terminal_flag(self):
        def events(document):
            return {
                event["name"]: event["terminal"]
                for event in document["paths"][self.STREAM]["post"]["x-sse-events"]
            }

        current = events(self.current)
        for name, terminal in events(self.baseline).items():
            self.assertIn(name, current, f"SSE event {name!r} was removed")
            self.assertEqual(current[name], terminal, f"SSE event {name!r} changed terminal")

    def test_requests_are_closed_and_responses_are_open(self):
        for name, schema in self.current["components"]["schemas"].items():
            if schema.get("type") != "object":
                continue
            if name in self.REQUESTS:
                self.assertIs(schema.get("additionalProperties"), False, name)
            else:
                self.assertIsNot(
                    schema.get("additionalProperties"),
                    False,
                    f"{name} is a response schema; closing it makes added fields breaking",
                )

    def test_every_sse_event_is_exposed_to_the_diff_with_its_schema(self):
        exposed = sse_as_operations.with_sse_operations(self.current)
        events = self.current["paths"][self.STREAM]["post"]["x-sse-events"]
        for path in self.current["paths"]:
            self.assertEqual(exposed["paths"][path], self.current["paths"][path])
        for event in events:
            operation = exposed["paths"][f"/x-sse-events/{event['name']}"]["get"]
            schema = operation["responses"]["200"]["content"]["application/json"]["schema"]
            self.assertEqual(schema, event["schema"])
        self.assertEqual(len(exposed["paths"]), len(self.current["paths"]) + len(events))
        self.assertNotIn("/x-sse-events/usage", self.current["paths"])

    def test_examples_still_reject_undocumented_fields(self):
        document = json.loads(generate.SOURCE.read_text())
        with self.assertRaises(AssertionError):
            generate._validate_schema(
                {"model": "m", "surprise": 1},
                {"$ref": "#/components/schemas/SseResponseStarted"},
                document,
                "example",
            )


if __name__ == "__main__":
    unittest.main()
