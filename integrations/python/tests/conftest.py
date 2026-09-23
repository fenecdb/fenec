"""The server the tests talk to: FENEC_URL and FENEC_TOKEN, as run-tests.sh
sets them for a fenec-pg it starts."""

import os
import uuid

import pytest

from fenecdb import Client

URL = os.environ.get("FENEC_URL", "http://127.0.0.1:8080")
TOKEN = os.environ.get("FENEC_TOKEN")


@pytest.fixture()
def client() -> Client:
    return Client(URL, TOKEN)


def fresh(prefix: str) -> str:
    """A collection name no other test uses."""
    return f"{prefix}_{uuid.uuid4().hex[:12]}"
