//! AI radio routes. Stubs until the radio integration is merged: they answer 503 `unavailable`.

use super::{
    auth::Member,
    error::{ApiError, ApiResult},
};

fn not_yet() -> ApiError {
    ApiError::unavailable("The AI radio is not available yet")
}

pub async fn start(_m: Member) -> ApiResult<()> {
    Err(not_yet())
}

pub async fn stop(_m: Member) -> ApiResult<()> {
    Err(not_yet())
}

pub async fn cancel(_m: Member) -> ApiResult<()> {
    Err(not_yet())
}
