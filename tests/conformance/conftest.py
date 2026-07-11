"""Conformance suite: real OpenAI Python SDK against the switchboard binary."""

from __future__ import annotations

import os
import shutil
import socket
import subprocess
import time
import uuid
from pathlib import Path

import httpx
import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
BINARY = REPO_ROOT / "target" / "debug" / "switchboard"
SBCTL = REPO_ROOT / "target" / "debug" / "sbctl"

TEST_TENANT_ID = os.environ.get(
    "TEST_TENANT_ID", "00000000-0000-7000-8000-000000000099"
)


def _free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def _postgres_up(url: str) -> bool:
    try:
        import urllib.parse as up

        p = up.urlparse(url)
        host = p.hostname or "127.0.0.1"
        port = p.port or 5432
        with socket.create_connection((host, port), timeout=1.0):
            return True
    except OSError:
        return False


def _redis_up(url: str) -> bool:
    try:
        import urllib.parse as up

        p = up.urlparse(url)
        host = p.hostname or "127.0.0.1"
        port = p.port or 6379
        with socket.create_connection((host, port), timeout=1.0):
            return True
    except OSError:
        return False


def _wait_for(url: str, timeout: float = 30.0) -> None:
    deadline = time.time() + timeout
    last_err = None
    while time.time() < deadline:
        try:
            r = httpx.get(url, timeout=1.0)
            if r.status_code == 200:
                return
            last_err = f"status {r.status_code}"
        except httpx.HTTPError as exc:
            last_err = str(exc)
        time.sleep(0.2)
    raise RuntimeError(f"server did not become ready at {url}: {last_err}")


def _terminate(proc: subprocess.Popen) -> None:
    if proc.poll() is not None:
        return
    try:
        proc.terminate()
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait(timeout=5)


def _spawn_server(*, require_auth: bool = False) -> tuple[str, subprocess.Popen]:
    if not BINARY.exists():
        pytest.skip("switchboard binary not built; run `cargo build` first")

    port = _free_port()
    env = os.environ.copy()
    env.setdefault("RUST_LOG", "switchboard=info")
    env["SB__SERVER__PORT"] = str(port)
    env["SB__AUTH__REQUIRE_AUTH"] = "true" if require_auth else "false"

    pg_url = os.environ.get(
        "DATABASE_URL",
        "postgres://switchboard:switchboard@127.0.0.1:5432/switchboard",
    )
    redis_url = os.environ.get("REDIS_URL", "redis://127.0.0.1:6379")

    if _postgres_up(pg_url):
        env["DATABASE_URL"] = pg_url
    else:
        env.pop("DATABASE_URL", None)

    if _redis_up(redis_url):
        env["REDIS_URL"] = redis_url
    else:
        env.pop("REDIS_URL", None)

    proc = subprocess.Popen(
        [str(BINARY)],
        cwd=REPO_ROOT,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    base = f"http://127.0.0.1:{port}"
    try:
        _wait_for(f"{base}/health")
        return base, proc
    except RuntimeError as exc:
        _terminate(proc)
        if proc.stderr:
            err = proc.stderr.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"server failed to start:\n{err}") from exc
        raise


@pytest.fixture(scope="session")
def server_url() -> str:
    external = os.environ.get("SWITCHBOARD_BASE_URL")
    if external:
        _wait_for(f"{external.rstrip('/')}/health")
        yield external.rstrip("/")
        return

    base, proc = _spawn_server(require_auth=False)
    try:
        yield base
    finally:
        _terminate(proc)


@pytest.fixture(scope="session")
def openai_client(server_url: str):
    from openai import OpenAI

    return OpenAI(base_url=f"{server_url}/v1", api_key="sk-test-conformance")


@pytest.fixture(scope="session")
def provider_ready(server_url: str) -> bool:
    try:
        r = httpx.get(f"{server_url}/ready", timeout=2.0)
        if r.status_code != 200:
            return False
        body = r.json()
        return bool(body.get("providers"))
    except httpx.HTTPError:
        return False


@pytest.fixture(scope="session")
def http_client(server_url: str) -> httpx.Client:
    return httpx.Client(base_url=server_url, timeout=30.0)


def _psql_run(sql: str) -> subprocess.CompletedProcess[str]:
    pg_url = os.environ.get("DATABASE_URL")
    if shutil.which("psql") and pg_url:
        return subprocess.run(
            ["psql", pg_url, "-v", "ON_ERROR_STOP=1", "-c", sql],
            check=True,
            capture_output=True,
            text=True,
        )
    return subprocess.run(
        [
            "docker",
            "compose",
            "exec",
            "-T",
            "postgres",
            "psql",
            "-U",
            "switchboard",
            "-d",
            "switchboard",
            "-v",
            "ON_ERROR_STOP=1",
            "-c",
            sql,
        ],
        cwd=REPO_ROOT,
        check=True,
        capture_output=True,
        text=True,
    )


@pytest.fixture(scope="session")
def third_party_api_key(server_url: str) -> str:
    existing = os.environ.get("THIRD_PARTY_API_KEY")
    if existing:
        return existing

    if not SBCTL.exists():
        pytest.skip("sbctl not built")

    pg_url = os.environ.get("DATABASE_URL")
    if not pg_url or not _postgres_up(pg_url):
        pytest.skip("postgres required for third-party key fixture")

    tenant_id = TEST_TENANT_ID
    _psql_run(
        f"INSERT INTO tenants (id, name) VALUES ('{tenant_id}', 'acme-corp') "
        "ON CONFLICT (id) DO NOTHING;"
    )

    out = subprocess.run(
        [str(SBCTL), "create-key", tenant_id, "pytest-third-party"],
        cwd=REPO_ROOT,
        env={**os.environ, "DATABASE_URL": pg_url},
        check=True,
        capture_output=True,
        text=True,
    )
    key = out.stdout.strip().splitlines()[-1]
    return key


@pytest.fixture(scope="session")
def third_party_client(server_url: str, third_party_api_key: str):
    from openai import OpenAI

    return OpenAI(base_url=f"{server_url}/v1", api_key=third_party_api_key)


@pytest.fixture(scope="session")
def rag_client(server_url: str, third_party_api_key: str) -> httpx.Client:
    return httpx.Client(
        base_url=server_url,
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        timeout=60.0,
    )


@pytest.fixture
def unique_doc_title() -> str:
    return f"conformance-doc-{uuid.uuid4().hex[:8]}"


@pytest.fixture(scope="module")
def auth_required_server():
    """Short-lived server with SB__AUTH__REQUIRE_AUTH=true for auth tests."""
    base, proc = _spawn_server(require_auth=True)
    try:
        yield base
    finally:
        _terminate(proc)
