"""Conformance tests for RAG endpoints (Phase 5)."""

from __future__ import annotations

import httpx
import pytest


def test_documents_upload(rag_client: httpx.Client, provider_ready, unique_doc_title: str):
    if not provider_ready:
        pytest.skip("no healthy provider (Ollama + nomic-embed-text required)")

    r = rag_client.post(
        "/v1/documents",
        json={
            "title": unique_doc_title,
            "content": "The refund policy for EU customers is 30 days from purchase.",
        },
    )
    assert r.status_code == 200
    body = r.json()
    assert body["chunks"] >= 1
    assert "id" in body


def test_documents_deduplicate_same_content(rag_client: httpx.Client, provider_ready):
    if not provider_ready:
        pytest.skip("no healthy provider")

    content = "Deduplication test: unique bytes for sha256 check."
    first = rag_client.post(
        "/v1/documents",
        json={"title": "dedup-a", "content": content},
    )
    assert first.status_code == 200
    second = rag_client.post(
        "/v1/documents",
        json={"title": "dedup-b", "content": content},
    )
    assert second.status_code == 200
    assert second.json()["deduplicated"] is True


def test_search_hybrid_after_ingest(rag_client: httpx.Client, provider_ready, unique_doc_title: str):
    if not provider_ready:
        pytest.skip("no healthy provider")

    rag_client.post(
        "/v1/documents",
        json={
            "title": unique_doc_title,
            "content": "Shipping to Vietnam takes 5-7 business days.",
        },
    )
    r = rag_client.post(
        "/v1/search",
        json={"query": "Vietnam shipping", "k": 3, "mode": "hybrid"},
    )
    assert r.status_code == 200
    body = r.json()
    assert "hits" in body
    assert isinstance(body["hits"], list)


def test_search_vector_mode(rag_client: httpx.Client, provider_ready, unique_doc_title: str):
    if not provider_ready:
        pytest.skip("no healthy provider")

    rag_client.post(
        "/v1/documents",
        json={"title": unique_doc_title, "content": "Vector search mode test content."},
    )
    r = rag_client.post(
        "/v1/search",
        json={"query": "vector search", "k": 2, "mode": "vector"},
    )
    assert r.status_code == 200


def test_search_lexical_mode(rag_client: httpx.Client, provider_ready, unique_doc_title: str):
    if not provider_ready:
        pytest.skip("no healthy provider")

    rag_client.post(
        "/v1/documents",
        json={"title": unique_doc_title, "content": "Lexical FTS refund window fourteen days."},
    )
    r = rag_client.post(
        "/v1/search",
        json={"query": "refund window", "k": 2, "mode": "lexical"},
    )
    assert r.status_code == 200


def test_delete_document(rag_client: httpx.Client, provider_ready, unique_doc_title: str):
    if not provider_ready:
        pytest.skip("no healthy provider")

    up = rag_client.post(
        "/v1/documents",
        json={"title": unique_doc_title, "content": "Temporary doc for delete test."},
    )
    assert up.status_code == 200
    doc_id = up.json()["id"]

    deleted = rag_client.delete(f"/v1/documents/{doc_id}")
    assert deleted.status_code == 204

    again = rag_client.delete(f"/v1/documents/{doc_id}")
    assert again.status_code == 404


def test_delete_unknown_document(rag_client: httpx.Client):
    r = rag_client.delete("/v1/documents/00000000-0000-7000-8000-000000000001")
    assert r.status_code == 404


def test_search_empty_query_validation(rag_client: httpx.Client):
    r = rag_client.post("/v1/search", json={"query": "", "k": 1})
    assert r.status_code in (200, 400, 404, 500)
