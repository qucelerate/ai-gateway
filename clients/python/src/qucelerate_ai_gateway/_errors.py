from __future__ import annotations

from typing import Any, Mapping, Optional


class GatewayError(Exception):
    """Base of every error this client raises."""


class ConfigError(GatewayError, ValueError):
    """The client or the call was configured with an invalid value."""


class ProblemError(GatewayError):
    """The gateway refused or failed the request with an RFC 9457 problem.

    `code` is an open string: match the codes you handle and fall back to
    `status` for the rest. `clients/conformance/errors.json` lists the codes of
    contract 1.0.0 and what each means for a retry.
    """

    def __init__(self, status: int, body: Mapping[str, Any], retry_after: Optional[int]) -> None:
        self.status = status
        self.code: str = body["code"]
        self.title: str = body.get("title", "")
        self.detail: str = body.get("detail", "")
        self.instance: str = body.get("instance", "")
        self.request_id: str = body.get("request_id", "")
        self.type: str = body.get("type", "")
        #: The Retry-After header in whole seconds, when the response had one.
        self.retry_after = retry_after
        super().__init__(f"gateway problem {status}: {self.code} ({self.title}; request {self.request_id})")


class HTTPError(GatewayError):
    """An error response without a problem body, for example from a proxy."""

    def __init__(self, status: int, request_id: Optional[str]) -> None:
        self.status = status
        self.request_id = request_id
        super().__init__(f"HTTP {status} without a problem body")


class StreamError(GatewayError):
    """A stream ended with a terminal error event. The request reached the
    provider, so it is never retried automatically."""

    def __init__(self, code: str, status: int) -> None:
        self.code = code
        self.status = status
        super().__init__(f"stream failed: {code} (status {status})")


class ProtocolError(GatewayError):
    """A stream closed before its terminal event. Streams are not resumable."""


class DecodeError(GatewayError):
    """A response body did not match the contract."""


class TransportError(GatewayError):
    """The request could not be sent or the response could not be read."""
