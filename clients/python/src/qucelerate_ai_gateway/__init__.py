"""A client for the Gateway inference API, contract 1.0.0.

    from qucelerate_ai_gateway import Client

    with Client("https://gateway.example.com", api_key) as client:
        response = client.chat("chat-default", [{"role": "user", "content": "Say hello."}])
        print(response.content)

The client is a transport wrapper: authorization, budgets, routing and
settlement stay on the gateway. Every call sends an Idempotency-Key (generated
unless you pass one) and retries automatically only when the gateway refused
the request before reserving anything.
"""

__version__ = "2026.11.0"

from ._async import AsyncChatStream, AsyncClient  # noqa: E402
from ._client import ChatStream, Client  # noqa: E402
from ._errors import (  # noqa: E402
    ConfigError,
    DecodeError,
    GatewayError,
    HTTPError,
    ProblemError,
    ProtocolError,
    StreamError,
    TransportError,
)
from ._types import (  # noqa: E402
    Batch,
    BatchCounts,
    BatchList,
    ChatResponse,
    Choice,
    ChoiceDelta,
    ChoiceFinished,
    Embedding,
    EmbeddingResponse,
    FileObject,
    Message,
    MessageResponse,
    ResponseCompleted,
    ResponseStarted,
    SafetyAnnotation,
    StreamEvent,
    Tool,
    ToolCall,
    ToolCallDelta,
    Usage,
    UsageReport,
)

__all__ = [
    "AsyncChatStream", "AsyncClient", "Batch", "BatchCounts", "BatchList", "ChatResponse", "ChatStream", "Choice", "ChoiceDelta",
    "ChoiceFinished", "Client", "ConfigError", "DecodeError", "Embedding", "EmbeddingResponse",
    "FileObject", "GatewayError", "HTTPError", "Message", "MessageResponse", "ProblemError", "ProtocolError",
    "ResponseCompleted", "ResponseStarted", "SafetyAnnotation", "StreamError", "StreamEvent",
    "Tool", "ToolCall", "ToolCallDelta", "TransportError", "Usage", "UsageReport", "__version__",
]
