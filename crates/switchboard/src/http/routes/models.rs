use crate::app::AppState;
use crate::http::wire::{ModelListResponse, ModelObject};
use axum::Json;
use axum::extract::{Path, State};

/// `GET /v1/models` — the catalogue, and the rate card.
///
/// Sorted by alias so the list is stable across restarts: a client diffing this response
/// to notice a new model should see a diff only when a model changes.
pub async fn list_models(State(state): State<AppState>) -> Json<ModelListResponse> {
    let mut data: Vec<ModelObject> = state
        .registry
        .model_registry
        .list()
        .map(ModelObject::from_spec)
        .collect();
    data.sort_by(|a, b| a.id.cmp(&b.id));
    Json(ModelListResponse::new(data))
}

pub async fn get_model(
    State(state): State<AppState>,
    Path(model_id): Path<String>,
) -> Result<Json<ModelObject>, crate::http::error::ApiError> {
    let spec = state
        .registry
        .model_registry
        .get(&model_id)
        .ok_or_else(|| crate::http::error::ApiError::ModelNotFound {
            model: model_id.clone(),
        })?;

    Ok(Json(ModelObject::from_spec(spec)))
}
