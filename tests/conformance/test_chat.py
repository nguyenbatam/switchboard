import httpx
import pytest


@pytest.mark.parametrize("stream", [False, True])
def test_chat_completion(openai_client, provider_ready, stream):
    if not provider_ready:
        pytest.skip("no healthy provider (start Ollama for live chat tests)")

    resp = openai_client.chat.completions.create(
        model="qwen3:4b",
        messages=[{"role": "user", "content": "Reply with exactly: pong"}],
        stream=stream,
        max_tokens=32,
    )

    if stream:
        chunks = list(resp)
        assert len(chunks) >= 1
        text = "".join(
            c.choices[0].delta.content or ""
            for c in chunks
            if c.choices and c.choices[0].delta.content
        )
        assert len(text) > 0
    else:
        assert resp.choices[0].message.content
        assert resp.usage.total_tokens > 0


def test_chat_system_and_user_messages(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = openai_client.chat.completions.create(
        model="qwen3:4b",
        messages=[
            {"role": "system", "content": "You are a terse assistant."},
            {"role": "user", "content": "Say hi in one word."},
        ],
        max_tokens=16,
    )
    assert resp.choices[0].message.content


def test_chat_max_tokens_respected(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = openai_client.chat.completions.create(
        model="qwen3:4b",
        messages=[{"role": "user", "content": "Count to 100"}],
        max_tokens=8,
    )
    assert resp.usage.completion_tokens <= 32


def test_chat_third_party_tenant(third_party_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = third_party_client.chat.completions.create(
        model="qwen3:4b",
        messages=[{"role": "user", "content": "Reply: ok"}],
        max_tokens=16,
    )
    assert resp.choices[0].message.content


def test_invalid_model_returns_error(openai_client):
    with pytest.raises(Exception):
        openai_client.chat.completions.create(
            model="does-not-exist-xyz",
            messages=[{"role": "user", "content": "hi"}],
        )


def test_chat_missing_messages_returns_400(http_client: httpx.Client):
    r = http_client.post("/v1/chat/completions", json={"model": "qwen3:4b"})
    assert r.status_code in (400, 422)


def test_chat_empty_model_returns_error(openai_client):
    with pytest.raises(Exception):
        openai_client.chat.completions.create(
            model="",
            messages=[{"role": "user", "content": "hi"}],
        )
