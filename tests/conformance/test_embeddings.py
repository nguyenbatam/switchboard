import httpx
import pytest


def test_embeddings(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider (start Ollama for live embedding tests)")

    resp = openai_client.embeddings.create(
        model="nomic-embed-text",
        input="hello switchboard",
    )
    assert len(resp.data) == 1
    assert len(resp.data[0].embedding) > 0


def test_embeddings_batch(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = openai_client.embeddings.create(
        model="nomic-embed-text",
        input=["first phrase", "second phrase"],
    )
    assert len(resp.data) == 2


def test_embeddings_vector_dimension(openai_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = openai_client.embeddings.create(
        model="nomic-embed-text",
        input="dimension check",
    )
    assert len(resp.data[0].embedding) >= 128


def test_embeddings_third_party_key(third_party_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = third_party_client.embeddings.create(
        model="nomic-embed-text",
        input="tenant isolated embedding",
    )
    assert len(resp.data[0].embedding) > 0


def test_embeddings_unknown_model(openai_client):
    with pytest.raises(Exception):
        openai_client.embeddings.create(
            model="no-embed-model",
            input="x",
        )


def test_embeddings_http_shape(http_client: httpx.Client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    r = http_client.post(
        "/v1/embeddings",
        json={"model": "nomic-embed-text", "input": "http test"},
    )
    assert r.status_code == 200
    body = r.json()
    assert body["object"] == "list"
    assert body["data"][0]["object"] == "embedding"
