import httpx
import pytest


def test_list_models(openai_client):
    models = openai_client.models.list()
    data = list(models)
    assert len(data) >= 1
    aliases = {m.id for m in data}
    assert "qwen3:4b" in aliases


def test_list_models_object_shape(openai_client):
    model = next(iter(openai_client.models.list()))
    assert model.object == "model"
    assert model.id
    assert model.owned_by


def test_list_models_includes_embed_model(openai_client):
    aliases = {m.id for m in openai_client.models.list()}
    assert "nomic-embed-text" in aliases


def test_get_model(openai_client):
    model = openai_client.models.retrieve("qwen3:4b")
    assert model.id == "qwen3:4b"


def test_get_model_object_field(openai_client):
    model = openai_client.models.retrieve("qwen3:4b")
    assert model.object == "model"


def test_get_unknown_model_returns_error(openai_client):
    with pytest.raises(Exception):
        openai_client.models.retrieve("does-not-exist-xyz")


def test_list_models_via_http(http_client: httpx.Client):
    r = http_client.get("/v1/models")
    assert r.status_code == 200
    body = r.json()
    assert body["object"] == "list"
    assert len(body["data"]) >= 1


def test_get_model_via_http(http_client: httpx.Client):
    r = http_client.get("/v1/models/qwen3:4b")
    assert r.status_code == 200
    assert r.json()["id"] == "qwen3:4b"


def test_get_model_http_404_shape(http_client: httpx.Client):
    r = http_client.get("/v1/models/no-such-model")
    assert r.status_code == 400
    body = r.json()
    assert body["error"]["type"] == "invalid_request_error"
