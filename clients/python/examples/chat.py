"""GATEWAY_URL=... GATEWAY_API_KEY=gw_... python examples/chat.py chat-default "Say hello."

Sends one chat call and one streamed call and prints the answers."""

import os
import sys

from qucelerate_ai_gateway import ChoiceDelta, Client, ResponseCompleted

model = sys.argv[1] if len(sys.argv) > 1 else "chat-default"
prompt = sys.argv[2] if len(sys.argv) > 2 else "Say hello."
messages = [{"role": "user", "content": prompt}]

with Client(os.environ["GATEWAY_URL"], os.environ["GATEWAY_API_KEY"]) as client:
    response = client.chat(model, messages, max_tokens=256)
    print("chat:", response.content)
    print("usage:", response.usage)
    print("stream: ", end="")
    with client.chat_stream(model, messages, max_tokens=256) as stream:
        for event in stream:
            if isinstance(event, ChoiceDelta):
                print(event.content_delta or "", end="", flush=True)
            elif isinstance(event, ResponseCompleted):
                print("\nusage:", event.usage)
