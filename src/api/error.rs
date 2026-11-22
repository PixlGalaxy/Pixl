//! `{ error: { code, message } }` responses and the JSON / query extractors that produce them.

use axum::{
    extract::{FromRequest, FromRequestParts, Query, Request},
    http::{request::Parts, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::de::DeserializeOwned;
use serde_json::json;
use tracing::warn;

use crate::perms::Deny;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
    /// "Every Lynx bot is busy": the invite link of the next bot to add (`error.invite`).
    pub invite: Option<String>,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            invite: None,
        }
    }

    pub fn with_invite(mut self, invite: Option<String>) -> Self {
        self.invite = invite;
        self
    }

    /// The jam's "only the host channel edits the shared queue" rule.
    pub fn jam_host_only() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "jam_host_only",
            "Only the jam host channel can change the shared queue",
        )
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid", message)
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "Sign in again")
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not_found", message)
    }

    pub fn no_permission() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "no_permission",
            "You don't have permission to do that",
        )
    }

    pub fn not_in_channel() -> Self {
        Self::new(StatusCode::FORBIDDEN, "not_in_channel", "Join the voice channel first")
    }

    pub fn history_disabled() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "history_disabled",
            "Turn on your listening history to use this",
        )
    }

    pub fn channel_not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "channel_not_allowed",
            "Lynx is not allowed in this channel",
        )
    }

    pub fn no_results() -> Self {
        Self::new(StatusCode::NOT_FOUND, "no_results", "Nothing found")
    }

    pub fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many requests, slow down",
        )
    }

    pub fn bot_unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "bot_unavailable", message)
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", message)
    }

    pub fn deny(d: Deny) -> Self {
        match d {
            Deny::NotInChannel => Self::not_in_channel(),
            Deny::DjOnly | Deny::ManagerOnly => Self::no_permission(),
        }
    }

    /// Unexpected failure: logged, never shown in detail.
    pub fn internal(context: &str, e: impl std::fmt::Display) -> Self {
        warn!(error = %e, "{context}");
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Something went wrong")
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut error = json!({ "code": self.code, "message": self.message });
        if let Some(invite) = self.invite {
            error["invite"] = json!(invite);
        }
        (self.status, Json(json!({ "error": error }))).into_response()
    }
}

/// `Json<T>` whose rejection is an `invalid` error.
pub struct Body<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(Self(v)),
            Err(e) => Err(ApiError::invalid(e.body_text())),
        }
    }
}

/// `Query<T>` whose rejection is an `invalid` error.
pub struct Params<T>(pub T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        match Query::<T>::from_request_parts(parts, state).await {
            Ok(Query(v)) => Ok(Self(v)),
            Err(e) => Err(ApiError::invalid(e.body_text())),
        }
    }
}

/// Parse a snowflake sent as a string.
pub fn snowflake(raw: &str) -> ApiResult<u64> {
    raw.trim()
        .parse::<u64>()
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(|| ApiError::invalid("invalid id"))
}
