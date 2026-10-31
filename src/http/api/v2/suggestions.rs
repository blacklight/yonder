//! `/api/v2/suggestions` — follow suggestions. Yonder has no
//! suggestion engine yet; an empty page is the honest, valid
//! response (Mastodon returns one when suggestions are disabled).

use std::sync::Arc;

use axum::{extract::State, Json};
use serde_json::{json, Value};

use crate::{
    auth::scopes::require_scope,
    http::{middleware::auth::AuthContext, AppState},
    Result,
};

/// `GET /api/v2/suggestions` — empty page.
#[utoipa::path(
    get,
    path = "/api/v2/suggestions",
    tag = "suggestions",
    responses(
        (status = 200, description = "Array of Suggestion entities (empty — no suggestion engine)"),
        (status = 401, description = "Missing or invalid token", body = crate::error::ErrorBody),
    )
)]
pub async fn list(State(state): State<Arc<AppState>>, auth: AuthContext) -> Result<Json<Value>> {
    require_scope(&state.config(), &auth, "read:accounts")?;
    Ok(Json(json!([])))
}
