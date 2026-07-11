use axum::extract::Request;
use axum::middleware::Next;
use axum::response::Response;
use tracing::Instrument;

pub async fn request_id_layer(req: Request, next: Next) -> Response {
    let request_id = req
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| format!("req_{}", uuid::Uuid::now_v7()));

    let span = tracing::info_span!(
        "http.request",
        request_id = %request_id,
        method = %req.method(),
        uri = %req.uri(),
    );

    async move {
        let mut resp = next.run(req).await;
        resp.headers_mut().insert(
            "x-request-id",
            http::HeaderValue::from_str(&request_id)
                .unwrap_or_else(|_| http::HeaderValue::from_static("unknown")),
        );
        resp
    }
    .instrument(span)
    .await
}
