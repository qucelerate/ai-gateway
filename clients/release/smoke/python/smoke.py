"""Installed-package smoke test: the released wheel imports, and a call
reaches its HTTP stack (a closed port: a transport error)."""
import sys

import qucelerate_ai_gateway
from qucelerate_ai_gateway import Client, TransportError

with Client("http://127.0.0.1:1", "gw_smoke") as client:
    try:
        client.chat("smoke", [{"role": "user", "content": "hi"}])
    except TransportError:
        print(f"ok: python client {qucelerate_ai_gateway.__version__} installs and runs")
        sys.exit(0)
    except Exception as error:  # noqa: BLE001 - any other outcome fails the smoke test
        sys.exit(f"expected a transport error, got {error!r}")
sys.exit("expected a transport error, the call succeeded")
