#![warn(clippy::pedantic)]
#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::module_name_repetitions,
    clippy::must_use_candidate
)]

pub mod config;
pub mod error;
pub mod handlers;
pub mod pin;
pub mod store;

pub use config::Config;
pub use error::ApiError;
pub use handlers::{PinResponse, router};
pub use pin::{Namespace, Pin};
pub use store::{PinKey, PinStore};
