use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::Serialize;

use crate::library::LibraryError;
use crate::player::PlayerError;
use crate::qobuz::QobuzError;
use crate::queue::QueueError;
use crate::upnp::UpnpError;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ErrorBody {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: String,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
    ) -> Self {
        Self {
            status,
            code: code.into(),
            message: message.into(),
            retryable,
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "INVALID_REQUEST", msg, false)
    }

    pub fn not_found(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::new(StatusCode::NOT_FOUND, code, msg, false)
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", msg, false)
    }

    pub fn internal(code: impl Into<String>, msg: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, code, msg, true)
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let env = ErrorEnvelope {
            error: ErrorBody {
                code: self.code,
                message: self.message,
                retryable: self.retryable,
                details: None,
            },
        };
        (self.status, Json(env)).into_response()
    }
}

impl From<QueueError> for ApiError {
    fn from(e: QueueError) -> Self {
        match e {
            QueueError::ZoneNotFound(id) => {
                ApiError::not_found("ZONE_NOT_FOUND", format!("zone {id:?} not found"))
            }
            QueueError::InvalidIndex => ApiError::bad_request("invalid queue index"),
            QueueError::InvalidArgument(m) => ApiError::bad_request(m),
            QueueError::EmptyQueue => ApiError::bad_request("queue is empty"),
            QueueError::Renderer(m) => {
                ApiError::new(StatusCode::BAD_GATEWAY, "RENDERER_ERROR", m, true)
            }
        }
    }
}

impl From<PlayerError> for ApiError {
    fn from(e: PlayerError) -> Self {
        match e {
            PlayerError::NotFound(id) => {
                ApiError::not_found("PLAYER_NOT_FOUND", format!("player {id:?} not found"))
            }
            PlayerError::Unauthorized => ApiError::unauthorized("invalid player token"),
            PlayerError::Disconnected => ApiError::new(
                StatusCode::CONFLICT,
                "PLAYER_DISCONNECTED",
                e.to_string(),
                true,
            ),
            PlayerError::CommandTimeout => ApiError::new(
                StatusCode::GATEWAY_TIMEOUT,
                "PLAYER_TIMEOUT",
                e.to_string(),
                true,
            ),
            PlayerError::PlaybackBlocked => ApiError::new(
                StatusCode::CONFLICT,
                "PLAYBACK_BLOCKED",
                e.to_string(),
                false,
            ),
            PlayerError::Invalid(m) => ApiError::bad_request(m),
        }
    }
}

impl From<LibraryError> for ApiError {
    fn from(e: LibraryError) -> Self {
        match e {
            LibraryError::NotFound => ApiError::not_found("NOT_FOUND", "resource not found"),
            LibraryError::ProviderNotFound(p) => {
                ApiError::not_found("PROVIDER_NOT_FOUND", format!("provider {p:?} not found"))
            }
            LibraryError::ProviderOffline(p) => ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "PROVIDER_OFFLINE",
                format!("provider {p:?} is not connected"),
                true,
            ),
            LibraryError::InvalidArgument(m) => ApiError::bad_request(m),
            other => ApiError::internal("LIBRARY_ERROR", other.to_string()),
        }
    }
}

impl From<QobuzError> for ApiError {
    fn from(e: QobuzError) -> Self {
        match e {
            QobuzError::Unauthenticated => ApiError::unauthorized("qobuz is not authenticated"),
            QobuzError::RateLimited => ApiError::new(
                StatusCode::TOO_MANY_REQUESTS,
                "QOBUZ_RATE_LIMITED",
                e.to_string(),
                true,
            ),
            QobuzError::NotFound => ApiError::not_found("NOT_FOUND", "qobuz resource not found"),
            QobuzError::InvalidQuality => ApiError::bad_request(e.to_string()),
            other => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "QOBUZ_ERROR",
                other.to_string(),
                true,
            ),
        }
    }
}

impl From<UpnpError> for ApiError {
    fn from(e: UpnpError) -> Self {
        match e {
            UpnpError::InvalidRendererUrl(m) => ApiError::bad_request(m),
            other => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "UPNP_ERROR",
                other.to_string(),
                true,
            ),
        }
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

pub fn parse_strict_json<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> ApiResult<T> {
    serde_json::from_slice(bytes).map_err(|e| ApiError::bad_request(format!("invalid JSON: {e}")))
}
