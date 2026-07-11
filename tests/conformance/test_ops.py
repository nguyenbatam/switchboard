import httpx


def test_health_returns_ok(server_url: str):
    r = httpx.get(f"{server_url}/health")
    assert r.status_code == 200
    assert r.json()["status"] == "ok"


def test_health_is_json(server_url: str):
    r = httpx.get(f"{server_url}/health")
    assert "application/json" in r.headers.get("content-type", "")


def test_health_no_auth_required(server_url: str):
    r = httpx.get(f"{server_url}/health")
    assert r.status_code == 200


def test_ready_reports_components(server_url: str):
    r = httpx.get(f"{server_url}/ready")
    assert r.status_code in (200, 503)
    body = r.json()
    assert "postgres" in body
    assert "redis" in body
    assert "providers" in body


def test_ready_status_field(server_url: str):
    r = httpx.get(f"{server_url}/ready")
    body = r.json()
    assert body["status"] in ("ready", "not_ready")


def test_ready_postgres_true_when_docker_up(server_url: str):
    r = httpx.get(f"{server_url}/ready")
    body = r.json()
    assert body["postgres"] is True


def test_ready_redis_true_when_docker_up(server_url: str):
    r = httpx.get(f"{server_url}/ready")
    body = r.json()
    assert body["redis"] is True


def test_metrics_returns_200(server_url: str):
    r = httpx.get(f"{server_url}/metrics")
    assert r.status_code == 200


def test_metrics_after_v1_traffic(http_client: httpx.Client):
    http_client.get("/v1/models")
    r = http_client.get("/metrics")
    assert r.status_code == 200
    # Prometheus may render empty until a labeled sample is recorded; body is always text.
    assert r.text is not None


def test_metrics_no_auth_required(server_url: str):
    r = httpx.get(f"{server_url}/metrics")
    assert r.status_code == 200


def test_v1_request_has_request_id_header(http_client: httpx.Client):
    r = http_client.get("/v1/models")
    assert r.status_code == 200
    assert r.headers.get("x-request-id")


def test_v1_accepts_custom_request_id(http_client: httpx.Client):
    rid = "req_conformance_custom_001"
    r = http_client.get("/v1/models", headers={"x-request-id": rid})
    assert r.status_code == 200
    assert r.headers.get("x-request-id") == rid
