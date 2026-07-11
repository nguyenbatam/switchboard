import time

import httpx
import pytest


def _await_at_least(client: httpx.Client, want: int, timeout_s: float) -> int:
    """Polls `/v1/usage` until `total_requests` reaches `want`, or the timeout expires.

    Returns whatever it last saw, so the assertion that follows reports the real number
    rather than a timeout.
    """
    deadline = time.monotonic() + timeout_s
    seen = 0
    while time.monotonic() < deadline:
        seen = client.get("/v1/usage").json()["total_requests"]
        if seen >= want:
            return seen
        time.sleep(0.1)
    return seen


def test_usage_via_http_before_requests(http_client: httpx.Client, third_party_api_key: str):
    client = httpx.Client(
        base_url=str(http_client.base_url),
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        timeout=30.0,
    )
    r = client.get("/v1/usage")
    assert r.status_code == 200
    body = r.json()
    assert "tenant_id" in body
    assert body["total_requests"] >= 0
    assert body["total_prompt_tokens"] >= 0
    assert body["total_completion_tokens"] >= 0


def test_usage_increments_after_chat(
    third_party_client, provider_ready, third_party_api_key: str, server_url: str
):
    if not provider_ready:
        pytest.skip("no healthy provider")

    client = httpx.Client(
        base_url=server_url,
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        timeout=30.0,
    )
    before = client.get("/v1/usage").json()["total_requests"]

    third_party_client.chat.completions.create(
        model="qwen3:4b",
        messages=[{"role": "user", "content": "Say: usage"}],
        max_tokens=8,
    )

    # The ledger lags, on purpose.
    #
    # The usage row is not written on the request path. It goes onto a bounded queue and a
    # background writer commits it in batches — 500 rows or 200 ms, whichever comes first
    # (docs/01 §5) — so that Postgres is never on a client's clock. Reading `/v1/usage` the
    # instant a completion returns therefore reads a ledger that is still catching up.
    #
    # Polling is not a workaround for a race. It is what any client must do, and it is what
    # `bot-fleet`'s `usage_settled()` exists to encapsulate.
    after = _await_at_least(client, before + 1, timeout_s=5.0)
    assert after >= before + 1


def test_usage_tenant_isolation(third_party_api_key: str, http_client: httpx.Client):
    client = httpx.Client(
        base_url=str(http_client.base_url),
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        timeout=30.0,
    )
    r = client.get("/v1/usage")
    body = r.json()
    assert body["tenant_id"] == "00000000-0000-7000-8000-000000000099"


def test_usage_cost_nano_field(http_client: httpx.Client, third_party_api_key: str):
    client = httpx.Client(
        base_url=str(http_client.base_url),
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        timeout=30.0,
    )
    r = client.get("/v1/usage")
    body = r.json()
    assert "total_cost_nano" in body
    assert isinstance(body["total_cost_nano"], int)


def test_usage_no_auth_uses_dev_tenant(http_client: httpx.Client):
    r = http_client.get("/v1/usage")
    assert r.status_code == 200
    assert r.json()["tenant_id"] == "00000000-0000-7000-8000-000000000001"
