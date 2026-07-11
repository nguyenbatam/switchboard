use super::chunk::Chunker;
use super::embed::{estimate_tokens, prefix_document};
use super::search::{SearchHit, SearchMode};
use crate::domain::embed::{EmbedInput, EmbedRequest};
use crate::providers::registry::ProviderRegistry;
use crate::providers::{CallCx, ProviderError};
use pgvector::Vector;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::sync::Arc;
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum RagError {
    #[error("database unavailable")]
    NoDatabase,
    #[error("embedding failed: {0}")]
    Embed(#[from] ProviderError),
    #[error("db error: {0}")]
    Db(#[from] sqlx::Error),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub struct IngestRequest {
    pub title: String,
    pub content: String,
    pub source_uri: Option<String>,
    pub metadata: Value,
    pub embed_model: String,
    pub chunk_tokens: Option<usize>,
    pub overlap_tokens: Option<usize>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct IngestResult {
    pub id: Uuid,
    pub chunks: usize,
    pub deduplicated: bool,
}

pub struct RagStore {
    pool: PgPool,
    registry: Arc<ProviderRegistry>,
    default_embed_model: String,
    chunk_tokens: usize,
    overlap_tokens: usize,
}

impl RagStore {
    pub fn new(
        pool: PgPool,
        registry: Arc<ProviderRegistry>,
        default_embed_model: String,
        chunk_tokens: usize,
        overlap_tokens: usize,
    ) -> Self {
        Self {
            pool,
            registry,
            default_embed_model,
            chunk_tokens,
            overlap_tokens,
        }
    }

    pub async fn ingest(
        &self,
        tenant_id: Uuid,
        req: IngestRequest,
        cx: &CallCx,
    ) -> Result<IngestResult, RagError> {
        let sha256 = Sha256::digest(req.content.as_bytes()).to_vec();

        if let Some(existing) = sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM documents WHERE tenant_id = $1 AND sha256 = $2",
        )
        .bind(tenant_id)
        .bind(&sha256)
        .fetch_optional(&self.pool)
        .await?
        {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM chunks WHERE document_id = $1")
                    .bind(existing)
                    .fetch_one(&self.pool)
                    .await?;
            return Ok(IngestResult {
                id: existing,
                chunks: count as usize,
                deduplicated: true,
            });
        }

        let embed_model = if req.embed_model.is_empty() {
            self.default_embed_model.clone()
        } else {
            req.embed_model.clone()
        };

        // The per-request override wins; otherwise the deployment's configured
        // defaults, which used to be ignored in favour of two hardcoded constants.
        let chunker = Chunker {
            chunk_tokens: req.chunk_tokens.unwrap_or(self.chunk_tokens),
            overlap_tokens: req.overlap_tokens.unwrap_or(self.overlap_tokens),
        };
        let texts = chunker.split(&req.title, &req.content);

        let prefixed: Vec<String> = texts.iter().map(|t| prefix_document(t)).collect();
        let embeddings = self.embed_batch(&embed_model, &prefixed, cx).await?;

        let doc_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO documents (id, tenant_id, source_uri, sha256, metadata)
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(doc_id)
        .bind(tenant_id)
        .bind(&req.source_uri)
        .bind(&sha256)
        .bind(&req.metadata)
        .execute(&mut *tx)
        .await?;

        for (ord, (text, embedding)) in texts.iter().zip(embeddings.iter()).enumerate() {
            let chunk_id = Uuid::now_v7();
            let vec = Vector::from(embedding.clone());
            sqlx::query(
                "INSERT INTO chunks (id, document_id, tenant_id, ord, content, token_count, embed_model, embedding)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(chunk_id)
            .bind(doc_id)
            .bind(tenant_id)
            .bind(ord as i32)
            .bind(text)
            .bind(estimate_tokens(text) as i16)
            .bind(&embed_model)
            .bind(vec)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;

        Ok(IngestResult {
            id: doc_id,
            chunks: texts.len(),
            deduplicated: false,
        })
    }

    pub async fn delete_document(&self, tenant_id: Uuid, doc_id: Uuid) -> Result<bool, RagError> {
        let result = sqlx::query("DELETE FROM documents WHERE id = $1 AND tenant_id = $2")
            .bind(doc_id)
            .bind(tenant_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    pub async fn search(
        &self,
        tenant_id: Uuid,
        query: &str,
        embed_model: &str,
        top_k: u32,
        mode: SearchMode,
        cx: &CallCx,
    ) -> Result<Vec<SearchHit>, RagError> {
        let model = if embed_model.is_empty() {
            &self.default_embed_model
        } else {
            embed_model
        };

        let started = std::time::Instant::now();
        let (stage, result) = match mode {
            SearchMode::Vector => (
                "vector",
                self.search_vector(tenant_id, query, model, top_k, cx).await,
            ),
            SearchMode::Lexical => (
                "lexical",
                self.search_lexical(tenant_id, query, top_k).await,
            ),
            SearchMode::Hybrid => (
                "hybrid",
                self.search_hybrid(tenant_id, query, model, top_k, cx).await,
            ),
        };
        crate::telemetry::metrics::record_retrieval(stage, started.elapsed().as_secs_f64());

        result
    }

    async fn embed_query(
        &self,
        model: &str,
        query: &str,
        cx: &CallCx,
    ) -> Result<Vec<f32>, RagError> {
        let prefixed = super::embed::prefix_query(query);
        let resp = self
            .registry
            .embed(
                model,
                &EmbedRequest {
                    model: model.into(),
                    input: EmbedInput::Single(prefixed),
                    encoding_format: None,
                    // Ingestion and retrieval are the gateway's own calls, made on a
                    // tenant's behalf but not on any one end user's.
                    user: None,
                },
                cx,
            )
            .await?;
        resp.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| RagError::Other("empty embedding response".into()))
    }

    async fn embed_batch(
        &self,
        model: &str,
        texts: &[String],
        cx: &CallCx,
    ) -> Result<Vec<Vec<f32>>, RagError> {
        const BATCH: usize = 64;
        let mut all = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            let resp = self
                .registry
                .embed(
                    model,
                    &EmbedRequest {
                        model: model.into(),
                        input: EmbedInput::Batch(batch.to_vec()),
                        encoding_format: None,
                        user: None,
                    },
                    cx,
                )
                .await?;
            let mut sorted = resp.data;
            sorted.sort_by_key(|d| d.index);
            for d in sorted {
                all.push(d.embedding);
            }
        }
        Ok(all)
    }

    async fn search_vector(
        &self,
        tenant_id: Uuid,
        query: &str,
        model: &str,
        top_k: u32,
        cx: &CallCx,
    ) -> Result<Vec<SearchHit>, RagError> {
        let embedding = self.embed_query(model, query, cx).await?;
        let vec = Vector::from(embedding);

        let rows = sqlx::query_as::<_, ChunkRow>(
            "SELECT c.id, c.document_id, c.ord, c.content,
                    (1 - (c.embedding <=> $1))::real AS score
             FROM chunks c
             WHERE c.tenant_id = $2 AND c.embed_model = $3
             ORDER BY c.embedding <=> $1
             LIMIT $4",
        )
        .bind(vec)
        .bind(tenant_id)
        .bind(model)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(ChunkRow::into_hit).collect())
    }

    async fn search_lexical(
        &self,
        tenant_id: Uuid,
        query: &str,
        top_k: u32,
    ) -> Result<Vec<SearchHit>, RagError> {
        let rows = sqlx::query_as::<_, ChunkRow>(
            "SELECT c.id, c.document_id, c.ord, c.content,
                    ts_rank_cd(c.content_tsv, q)::real AS score
             FROM chunks c, websearch_to_tsquery('english', $1) q
             WHERE c.tenant_id = $2 AND c.content_tsv @@ q
             ORDER BY score DESC
             LIMIT $3",
        )
        .bind(query)
        .bind(tenant_id)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(ChunkRow::into_hit).collect())
    }

    async fn search_hybrid(
        &self,
        tenant_id: Uuid,
        query: &str,
        model: &str,
        top_k: u32,
        cx: &CallCx,
    ) -> Result<Vec<SearchHit>, RagError> {
        let embedding = self.embed_query(model, query, cx).await?;
        let vec = Vector::from(embedding);
        const RRF_K: f32 = 60.0;
        const CANDIDATES: i64 = 50;

        let rows = sqlx::query_as::<_, ChunkRow>(
            "WITH vec AS (
                SELECT id, row_number() OVER (ORDER BY embedding <=> $1) AS rank
                FROM chunks
                WHERE tenant_id = $2 AND embed_model = $3
                ORDER BY embedding <=> $1
                LIMIT $4
             ),
             fts AS (
                SELECT c.id, row_number() OVER (
                    ORDER BY ts_rank_cd(c.content_tsv, q) DESC
                ) AS rank
                FROM chunks c, websearch_to_tsquery('english', $5) q
                WHERE c.tenant_id = $2 AND c.content_tsv @@ q
                ORDER BY 2
                LIMIT $4
             )
             SELECT c.id, c.document_id, c.ord, c.content,
                    (COALESCE(1.0 / ($6 + vec.rank), 0) + COALESCE(1.0 / ($6 + fts.rank), 0))::real AS score
             FROM vec
             FULL OUTER JOIN fts USING (id)
             JOIN chunks c ON c.id = COALESCE(vec.id, fts.id)
             ORDER BY score DESC
             LIMIT $7",
        )
        .bind(vec)
        .bind(tenant_id)
        .bind(model)
        .bind(CANDIDATES)
        .bind(query)
        .bind(RRF_K)
        .bind(top_k as i64)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows.into_iter().map(ChunkRow::into_hit).collect())
    }
}

#[derive(sqlx::FromRow)]
struct ChunkRow {
    id: Uuid,
    document_id: Uuid,
    ord: i32,
    content: String,
    score: f32,
}

impl ChunkRow {
    fn into_hit(self) -> SearchHit {
        SearchHit {
            citation: SearchHit::citation_for(self.document_id, self.ord),
            chunk_id: self.id,
            document_id: self.document_id,
            ord: self.ord,
            content: self.content,
            score: self.score,
        }
    }
}
