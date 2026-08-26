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
    NamespaceFull,
    TooManyGuesses,
    NotReady,
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
            Self::NoCapacity | Self::NamespaceFull | Self::NotReady => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::TooManyGuesses => StatusCode::TOO_MANY_REQUESTS,
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
            Self::NamespaceFull => "This namespace has too many live pins.".to_string(),
            Self::TooManyGuesses => "Too many unknown pins requested; slow down.".to_string(),
            Self::NotReady => "Service is not ready.".to_string(),
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
            // Every backpressure case tells the caller when to come back.
            Self::NoCapacity | Self::NamespaceFull | Self::TooManyGuesses | Self::NotReady => (
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
            (ApiError::NamespaceFull, 503),
            (ApiError::TooManyGuesses, 429),
            (ApiError::NotReady, 503),
        ];
        for (error, expected) in cases {
            assert_eq!(error.status().as_u16(), expected, "{error:?}");
        }
    }

    #[test]
    fn backpressure_errors_carry_retry_after() {
        for error in [
            ApiError::NoCapacity,
            ApiError::NamespaceFull,
            ApiError::TooManyGuesses,
            ApiError::NotReady,
        ] {
            let response = error.clone().into_response();
            assert_eq!(
                response.headers().get(header::RETRY_AFTER),
                Some(&header::HeaderValue::from_static(RETRY_AFTER_SECS)),
                "{error:?} should tell the caller when to retry"
            );
        }
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
