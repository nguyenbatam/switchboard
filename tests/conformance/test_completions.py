import httpx
import pytest


def test_legacy_completions_non_stream(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = openai_client.completions.create(
        model="qwen3:4b",
        prompt="Say hello in one word.",
        max_tokens=16,
    )
    assert resp.choices[0].text
    assert resp.usage.total_tokens > 0


def test_legacy_completions_stream(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    stream = openai_client.completions.create(
        model="qwen3:4b",
        prompt="Count: one two",
        max_tokens=16,
        stream=True,
    )
    chunks = list(stream)
    assert len(chunks) >= 1


def test_legacy_completions_maps_to_chat(http_client: httpx.Client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    r = http_client.post(
        "/v1/completions",
        json={"model": "qwen3:4b", "prompt": "Reply: yes", "max_tokens": 8},
    )
    assert r.status_code == 200
    body = r.json()
    assert body["choices"][0]["text"] is not None


def test_legacy_completions_invalid_model(openai_client):
    with pytest.raises(Exception):
        openai_client.completions.create(
            model="missing-model-xyz",
            prompt="hi",
        )


def test_legacy_completions_missing_prompt(http_client: httpx.Client):
    r = http_client.post("/v1/completions", json={"model": "qwen3:4b"})
    assert r.status_code in (400, 422, 500)
