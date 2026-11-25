"""A JSON response that does not match the contract is a DecodeError."""

import httpx
import pytest

from qucelerate_ai_gateway import ChatResponse, DecodeError, EmbeddingResponse
from qucelerate_ai_gateway._client import _decode

CHAT = {"id": "c", "model": "m", "choices": [], "usage": None}
EMBEDDINGS = {"object": "list", "model": "m", "data": [], "usage": None}


@pytest.mark.parametrize(
    "build, body",
    [
        (ChatResponse.from_dict, {**CHAT, "usage": []}),
        (ChatResponse.from_dict, {**CHAT, "choices": ["x"]}),
        (ChatResponse.from_dict, {**CHAT, "choices": None}),
        (ChatResponse.from_dict, []),
        (EmbeddingResponse.from_dict, {**EMBEDDINGS, "usage": []}),
        (EmbeddingResponse.from_dict, {**EMBEDDINGS, "data": [1]}),
    ],
)
def test_a_malformed_body_is_a_decode_error(build, body):
    with pytest.raises(DecodeError):
        response = httpx.Response(200, json=body)
        _decode(build, response, response.content)
