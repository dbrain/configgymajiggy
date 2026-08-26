use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Json, Response};
use serde::Serialize;

use crate::pin::Invalid;

const RETRY_AFTER_SECS: &str = "5";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    InvalidNamespace,
    InvalidPin,
    PinNotFound,
    PinAlreadyPopulated,
    PayloadTooLarge(usize),
    MalformedPayload,
    NoCapacity,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

impl ApiError {
    fn status(&self) -> StatusCode {
        match self {
            Self::InvalidNamespace | Self::InvalidPin => StatusCode::BAD_REQUEST,
            Self::PinNotFound => StatusCode::NOT_FOUND,
            Self::PinAlreadyPopulated => StatusCode::CONFLICT,
            Self::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            Self::MalformedPayload => StatusCode::UNPROCESSABLE_ENTITY,
            Self::NoCapacity => StatusCode::SERVICE_UNAVAILABLE,
        }
    }

    fn message(&self) -> String {
        match self {
            Self::InvalidNamespace => {
                "Namespace must be 1-64 characters of A-Z, a-z, 0-9, '-' or '_'.".to_string()
            }
            Self::InvalidPin => "Pin is not a well-formed pin for this service.".to_string(),
            Self::PinNotFound => "Pin not found or expired.".to_string(),
            Self::PinAlreadyPopulated => "Pin already holds an undelivered payload.".to_string(),
            Self::PayloadTooLarge(limit) => format!("Payload exceeds {limit} bytes."),
            Self::MalformedPayload => "Body must be a JSON object.".to_string(),
            Self::NoCapacity => "No pin available right now.".to_string(),
        }
    }
}

impl From<Invalid> for ApiError {
    fn from(invalid: Invalid) -> Self {
        match invalid {
            Invalid::Namespace => Self::InvalidNamespace,
            Invalid::Pin => Self::InvalidPin,
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = Json(ErrorBody {
            error: self.message(),
        });

        match self {
            Self::NoCapacity => (
                self.status(),
                [(header::RETRY_AFTER, RETRY_AFTER_SECS)],
                body,
            )
                .into_response(),
            _ => (self.status(), body).into_response(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_match_the_documented_table() {
        let cases = [
            (ApiError::InvalidNamespace, 400),
            (ApiError::InvalidPin, 400),
            (ApiError::PinNotFound, 404),
            (ApiError::PinAlreadyPopulated, 409),
            (ApiError::PayloadTooLarge(3000), 413),
            (ApiError::MalformedPayload, 422),
            (ApiError::NoCapacity, 503),
        ];
        for (error, expected) in cases {
            assert_eq!(error.status().as_u16(), expected, "{error:?}");
        }
    }

    #[test]
    fn capacity_errors_carry_retry_after() {
        let response = ApiError::NoCapacity.into_response();
        assert_eq!(
            response.headers().get(header::RETRY_AFTER).unwrap(),
            RETRY_AFTER_SECS
        );
    }

    #[test]
    fn messages_never_echo_user_input() {
        // Namespaces are attacker-controlled; nothing user-supplied may be
        // reflected into an error body.
        for error in [
            ApiError::InvalidNamespace,
            ApiError::InvalidPin,
            ApiError::PinNotFound,
        ] {
            assert!(!error.message().contains('{'));
        }
    }
}
