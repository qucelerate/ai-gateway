"""Response and event types of the inference contract 1.0.0.

Parsing ignores fields this version does not know, as contract 1.x requires;
`to_dict()` gives the wire form of the fields it does know.
"""

from __future__ import annotations

import dataclasses
from dataclasses import dataclass, field
from typing import Any, ClassVar, Dict, List, Mapping, Optional, Union


def _wire(value: Any) -> Any:
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return value.to_dict()
    if isinstance(value, list):
        return [_wire(item) for item in value]
    return value


class _Wire:
    _skip: ClassVar[frozenset] = frozenset()

    def to_dict(self) -> Dict[str, Any]:
        return {
            item.name: _wire(getattr(self, item.name))
            for item in dataclasses.fields(self)  # type: ignore[arg-type]
            if item.name not in self._skip
        }


@dataclass
class ToolCall(_Wire):
    """A call the model made to one of the request's tools. Send it back
    unchanged in the assistant message's ``tool_calls``, and answer it with a
    ``tool`` message whose ``tool_call_id`` is its ``id``. ``arguments`` is a
    JSON object, serialized, as the model wrote it: validate it before acting
    on it."""

    id: str
    name: str
    arguments: str

    def to_dict(self) -> Dict[str, Any]:
        return {"id": self.id, "type": "function",
                "function": {"name": self.name, "arguments": self.arguments}}

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "ToolCall":
        function = data.get("function") or {}
        return cls(data["id"], function["name"], function.get("arguments") or "")


ToolCallLike = Union[ToolCall, Mapping[str, Any]]


@dataclass
class Message(_Wire):
    """One message of the request. `role` is system, developer, user,
    assistant or tool. An assistant message that called tools carries its
    `tool_calls` (its `content` may then be None); a tool message answers one
    of them by `tool_call_id`."""

    role: str
    content: Optional[str] = None
    tool_calls: Optional[List[ToolCallLike]] = None
    tool_call_id: Optional[str] = None

    def to_dict(self) -> Dict[str, Any]:
        wire: Dict[str, Any] = {"role": self.role, "content": self.content}
        if self.tool_calls:
            wire["tool_calls"] = [_wire(call) for call in self.tool_calls]
        elif self.content is None:
            raise ValueError("a message without tool_calls needs content")
        if self.tool_call_id is not None:
            wire["tool_call_id"] = self.tool_call_id
        return wire


@dataclass
class Tool(_Wire):
    """A function the model may call. `parameters` is a JSON Schema whose
    ``type`` is ``object``; None sends none (the gateway takes an empty
    object)."""

    name: str
    description: Optional[str] = None
    parameters: Optional[Dict[str, Any]] = None

    def to_dict(self) -> Dict[str, Any]:
        function: Dict[str, Any] = {"name": self.name}
        if self.description is not None:
            function["description"] = self.description
        if self.parameters is not None:
            function["parameters"] = self.parameters
        return {"type": "function", "function": function}


ToolLike = Union[Tool, Mapping[str, Any]]
#: ``"auto"``, ``"none"``, ``"required"``, or ``{"type": "function",
#: "function": {"name": ...}}`` to make the model call that tool.
ToolChoice = Union[str, Mapping[str, Any]]
MessageLike = Union[Message, Mapping[str, Any]]


@dataclass
class Usage(_Wire):
    """Token usage. A counter the provider did not report is None, never 0.

    `reasoning_tokens` is counted apart from `output_tokens`; None means the
    provider does not report reasoning, which is then inside `output_tokens`.
    `quality` is exact, estimated or unknown; `completeness` is complete,
    partial or unavailable.
    """

    input_tokens: Optional[int]
    output_tokens: Optional[int]
    cached_input_tokens: Optional[int]
    reasoning_tokens: Optional[int]
    other_tokens: Optional[int]
    quality: str
    completeness: str

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "Usage":
        return cls(
            input_tokens=data.get("input_tokens"),
            output_tokens=data.get("output_tokens"),
            cached_input_tokens=data.get("cached_input_tokens"),
            reasoning_tokens=data.get("reasoning_tokens"),
            other_tokens=data.get("other_tokens"),
            quality=data["quality"],
            completeness=data["completeness"],
        )


def _usage(data: Any) -> Optional[Usage]:
    return None if data is None else Usage.from_dict(data)


@dataclass
class MessageResponse(_Wire):
    role: str
    #: Empty when the model returned only tool calls or a refusal.
    content: str
    name: Optional[str] = None
    tool_call_id: Optional[str] = None
    #: The calls the model made; empty when it made none (`finish_reason`
    #: is then not ``tool_calls``).
    tool_calls: List[ToolCall] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "MessageResponse":
        return cls(data["role"], data.get("content") or "", data.get("name"), data.get("tool_call_id"),
                   [ToolCall.from_dict(call) for call in data.get("tool_calls") or []])

    def to_message(self) -> Message:
        """This response as the assistant message that continues the
        conversation, its tool calls included."""
        return Message("assistant", self.content if self.content or not self.tool_calls else None,
                       list(self.tool_calls) or None)


@dataclass
class Choice(_Wire):
    index: int
    message: MessageResponse
    #: An open string (stop, length, tool_calls, ...): handle values you do not know.
    finish_reason: Optional[str]
    tool_calls: List[Dict[str, Any]] = field(default_factory=list)
    refusal: Optional[str] = None
    safety: List[Dict[str, Any]] = field(default_factory=list)
    annotations: List[Dict[str, Any]] = field(default_factory=list)

    @classmethod
    def from_dict(cls, data: Mapping[str, Any]) -> "Choice":
        return cls(
            index=data["index"],
            message=MessageResponse.from_dict(data["message"]),
            finish_reason=data.get("finish_reason"),
            tool_calls=list(data.get("tool_calls") or []),
            refusal=data.get("refusal"),
            safety=list(data.get("safety") or []),
            annotations=list(data.get("annotations") or []),
        )


@dataclass
class ChatResponse(_Wire):
    id: str
    model: str
    choices: List[Choice]
    usage: Optional[Usage]
    #: The X-Request-Id response header: quote it when reporting a problem.
    request_id: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], request_id: Optional[str] = None) -> "ChatResponse":
        return cls(
            id=data["id"],
            model=data["model"],
            choices=[Choice.from_dict(choice) for choice in data["choices"]],
            usage=_usage(data.get("usage")),
            request_id=request_id,
        )

    @property
    def content(self) -> Optional[str]:
        """The first choice's message content, the usual answer."""
        return self.choices[0].message.content if self.choices else None


@dataclass
class Embedding(_Wire):
    object: str
    index: int
    embedding: List[float]


@dataclass
class EmbeddingResponse(_Wire):
    object: str
    model: str
    data: List[Embedding]
    usage: Optional[Usage]
    request_id: Optional[str] = None

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], request_id: Optional[str] = None) -> "EmbeddingResponse":
        return cls(
            object=data["object"],
            model=data["model"],
            data=[Embedding(item["object"], item["index"], list(item["embedding"])) for item in data["data"]],
            usage=_usage(data.get("usage")),
            request_id=request_id,
        )

    @property
    def vectors(self) -> List[List[float]]:
        """The vectors in input order."""
        return [item.embedding for item in sorted(self.data, key=lambda item: item.index)]


class StreamEvent(_Wire):
    """Base of the events a chat stream yields. `event` is the SSE event name."""

    event: ClassVar[str]
    _skip = frozenset({"event"})


@dataclass
class ResponseStarted(StreamEvent):
    event: ClassVar[str] = "response.started"
    model: str = ""


@dataclass
class ChoiceDelta(StreamEvent):
    event: ClassVar[str] = "choice.delta"
    choice_index: int = 0
    content_delta: Optional[str] = None
    refusal_delta: Optional[str] = None
    annotations: List[Dict[str, Any]] = field(default_factory=list)


@dataclass
class ToolCallDelta(StreamEvent):
    """Part of a tool call; `id` and `name` arrive in its first delta only."""

    event: ClassVar[str] = "tool_call.delta"
    choice_index: int = 0
    tool_call_index: int = 0
    id: Optional[str] = None
    name: Optional[str] = None
    arguments_delta: str = ""


@dataclass
class ChoiceFinished(StreamEvent):
    event: ClassVar[str] = "choice.finished"
    choice_index: int = 0
    finish_reason: str = ""


@dataclass
class SafetyAnnotation(StreamEvent):
    event: ClassVar[str] = "safety.annotation"
    choice_index: int = 0
    category: str = ""
    blocked: bool = False


@dataclass
class UsageReport(StreamEvent):
    """The `usage` event."""

    event: ClassVar[str] = "usage"
    usage: Optional[Usage] = None

    def to_dict(self) -> Dict[str, Any]:
        return self.usage.to_dict() if self.usage else {}


@dataclass
class ResponseCompleted(StreamEvent):
    """The last event of a successful stream."""

    event: ClassVar[str] = "response.completed"
    model: str = ""
    usage: Optional[Usage] = None


def _fields(cls: type, data: Mapping[str, Any]) -> Dict[str, Any]:
    names = {item.name for item in dataclasses.fields(cls)}
    return {key: value for key, value in data.items() if key in names}


#: The stream events contract 1.0.0 defines, besides the terminal error.
KNOWN_EVENTS = frozenset(
    {"response.started", "choice.delta", "tool_call.delta", "choice.finished", "safety.annotation", "usage", "response.completed"}
)


def decode_event(name: str, data: Mapping[str, Any]) -> Optional[StreamEvent]:
    """The typed event, or None for an event this version does not know."""
    if name == "usage":
        return UsageReport(Usage.from_dict(data))
    if name == "response.completed":
        return ResponseCompleted(model=data["model"], usage=_usage(data.get("usage")))
    for cls in (ResponseStarted, ChoiceDelta, ToolCallDelta, ChoiceFinished, SafetyAnnotation):
        if cls.event == name:
            return cls(**_fields(cls, data))
    return None


@dataclass
class FileObject(_Wire):
    """An uploaded batch input file, or a batch's output or error file."""

    id: str
    bytes: int
    filename: str
    purpose: str
    created_at: int
    request_id: Optional[str] = None

    _skip: ClassVar[frozenset] = frozenset({"request_id"})

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], request_id: Optional[str] = None) -> "FileObject":
        return cls(
            id=data["id"],
            bytes=data["bytes"],
            filename=data["filename"],
            purpose=data["purpose"],
            created_at=data["created_at"],
            request_id=request_id,
        )


@dataclass
class BatchCounts(_Wire):
    total: int
    completed: int
    failed: int


@dataclass
class Batch(_Wire):
    """A batch: OpenAI's batch object. `status` is an open string
    (validating, in_progress, finalizing, completed, failed, expired,
    cancelling, cancelled). `gateway` is the gateway's own account, on a
    single-batch read: each job's `mode` (native or emulated) and
    `price_source` (batch or on_demand), and `cost` per currency."""

    id: str
    status: str
    endpoint: str
    input_file_id: str
    output_file_id: Optional[str]
    error_file_id: Optional[str]
    created_at: int
    expires_at: int
    request_counts: BatchCounts
    metadata: Dict[str, str]
    gateway: Optional[Dict[str, Any]] = None
    request_id: Optional[str] = None

    _skip: ClassVar[frozenset] = frozenset({"request_id"})

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], request_id: Optional[str] = None) -> "Batch":
        counts = data["request_counts"]
        return cls(
            id=data["id"],
            status=data["status"],
            endpoint=data["endpoint"],
            input_file_id=data["input_file_id"],
            output_file_id=data.get("output_file_id"),
            error_file_id=data.get("error_file_id"),
            created_at=data["created_at"],
            expires_at=data["expires_at"],
            request_counts=BatchCounts(counts["total"], counts["completed"], counts["failed"]),
            metadata=dict(data.get("metadata") or {}),
            gateway=data.get("gateway"),
            request_id=request_id,
        )

    @property
    def done(self) -> bool:
        """Whether the batch has ended: its output and error files are final."""
        return self.status in ("completed", "failed", "expired", "cancelled")


@dataclass
class BatchList(_Wire):
    data: List[Batch]
    has_more: bool
    request_id: Optional[str] = None

    _skip: ClassVar[frozenset] = frozenset({"request_id"})

    @classmethod
    def from_dict(cls, data: Mapping[str, Any], request_id: Optional[str] = None) -> "BatchList":
        return cls(
            data=[Batch.from_dict(item) for item in data["data"]],
            has_more=bool(data.get("has_more")),
            request_id=request_id,
        )
