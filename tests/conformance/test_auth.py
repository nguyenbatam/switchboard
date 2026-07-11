"""Auth flows: tenant provisioning, API keys, 3rd-party vs dev tenant."""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import httpx
import pytest

REPO_ROOT = Path(__file__).resolve().parents[2]
SBCTL = REPO_ROOT / "target" / "debug" / "sbctl"
DEV_TENANT_ID = "00000000-0000-7000-8000-000000000001"


def _psql_run(sql: str) -> None:
    pg_url = os.environ.get("DATABASE_URL")
    if not pg_url:
        pytest.skip("DATABASE_URL not set")
    import shutil

    if shutil.which("psql"):
        subprocess.run(
            ["psql", pg_url, "-v", "ON_ERROR_STOP=1", "-c", sql],
            check=True,
            capture_output=True,
            text=True,
        )
    else:
        subprocess.run(
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


def test_third_party_key_format(third_party_api_key: str):
    assert third_party_api_key.startswith("sk-sb-")
    assert "." in third_party_api_key


def test_third_party_key_chat_isolated(third_party_client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    resp = third_party_client.chat.completions.create(
        model="qwen3:4b",
        messages=[{"role": "user", "content": "Reply: tenant-ok"}],
        max_tokens=12,
    )
    assert resp.choices[0].message.content


def test_invalid_key_without_require_auth_falls_back_to_dev(http_client: httpx.Client):
    r = httpx.Client(
        base_url=str(http_client.base_url),
        headers={"Authorization": "Bearer sk-sb-invalid.invalidsecret"},
        timeout=30.0,
    ).get("/v1/usage")
    assert r.status_code == 200
    assert r.json()["tenant_id"] == DEV_TENANT_ID


def test_auth_required_rejects_missing_key(auth_required_server: str):
    r = httpx.post(
        f"{auth_required_server}/v1/chat/completions",
        json={
            "model": "qwen3:4b",
            "messages": [{"role": "user", "content": "hi"}],
        },
        timeout=10.0,
    )
    assert r.status_code == 401
    assert r.json()["error"]["type"] == "authentication_error"


def test_auth_required_rejects_bad_key(auth_required_server: str):
    r = httpx.post(
        f"{auth_required_server}/v1/chat/completions",
        headers={"Authorization": "Bearer sk-sb-deadbeef.badsecret"},
        json={
            "model": "qwen3:4b",
            "messages": [{"role": "user", "content": "hi"}],
        },
        timeout=10.0,
    )
    assert r.status_code == 401


def test_auth_required_accepts_valid_key(
    auth_required_server: str, third_party_api_key: str, provider_ready
):
    if not provider_ready:
        pytest.skip("no healthy provider")

    r = httpx.post(
        f"{auth_required_server}/v1/chat/completions",
        headers={"Authorization": f"Bearer {third_party_api_key}"},
        json={
            "model": "qwen3:4b",
            "messages": [{"role": "user", "content": "Say ok"}],
            "max_tokens": 8,
        },
        timeout=60.0,
    )
    assert r.status_code == 200
    assert r.json()["choices"][0]["message"]["content"]


def test_sbctl_create_key_for_new_tenant(server_url: str, provider_ready):
    if not SBCTL.exists():
        pytest.skip("sbctl not built")

    tenant_id = "00000000-0000-7000-8000-000000000098"
    _psql_run(
        f"INSERT INTO tenants (id, name) VALUES ('{tenant_id}', 'register-flow') "
        "ON CONFLICT (id) DO NOTHING;"
    )
    out = subprocess.run(
        [str(SBCTL), "create-key", tenant_id, "onboarding"],
        check=True,
        capture_output=True,
        text=True,
    )
    api_key = out.stdout.strip().splitlines()[-1]

    if not provider_ready:
        pytest.skip("no healthy provider")

    r = httpx.post(
        f"{server_url}/v1/chat/completions",
        headers={"Authorization": f"Bearer {api_key}"},
        json={
            "model": "qwen3:4b",
            "messages": [{"role": "user", "content": "ping"}],
            "max_tokens": 4,
        },
        timeout=60.0,
    )
    assert r.status_code == 200
    assert r.json()["choices"]
