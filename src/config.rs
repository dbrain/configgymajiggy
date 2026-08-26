use std::env;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

pub const DEFAULT_BIND_ADDRESS: &str = "0.0.0.0:8080";
pub const DEFAULT_PIN_LENGTH: usize = 4;
pub const DEFAULT_MAX_PAYLOAD_BYTES: usize = 3000;
pub const DEFAULT_STALE_AGE_MINS: u64 = 10;
pub const DEFAULT_CLEANUP_INTERVAL_SECS: u64 = 10;
pub const DEFAULT_MAX_ENTRIES: usize = 100_000;
pub const DEFAULT_REQUEST_TIMEOUT_SECS: u64 = 30;

/// Upper bound on `PIN_LENGTH`. Guards against a typo turning every allocation
/// into a multi-kilobyte key.
const PIN_LENGTH_LIMIT: usize = 64;

#[derive(Debug, Clone)]
pub struct Config {
    pub bind_address: String,
    pub pin_length: usize,
    pub max_payload_bytes: usize,
    pub stale_age: Duration,
    pub cleanup_interval: Duration,
    pub max_entries: usize,
    pub request_timeout: Duration,
    pub allowed_origins: Vec<String>,
}

pub struct ConfigError {
    key: &'static str,
    reason: String,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.key, self.reason)
    }
}

// `main` returns Box<dyn Error>, which reports with Debug; delegating keeps the
// startup failure readable instead of a struct dump.
impl fmt::Debug for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl std::error::Error for ConfigError {}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_address: DEFAULT_BIND_ADDRESS.to_string(),
            pin_length: DEFAULT_PIN_LENGTH,
            max_payload_bytes: DEFAULT_MAX_PAYLOAD_BYTES,
            stale_age: Duration::from_secs(DEFAULT_STALE_AGE_MINS * 60),
            cleanup_interval: Duration::from_secs(DEFAULT_CLEANUP_INTERVAL_SECS),
            max_entries: DEFAULT_MAX_ENTRIES,
            request_timeout: Duration::from_secs(DEFAULT_REQUEST_TIMEOUT_SECS),
            allowed_origins: Vec::new(),
        }
    }
}

impl Config {
    pub fn from_env() -> Result<Self, ConfigError> {
        let config = Self {
            bind_address: env::var("BIND_ADDRESS")
                .unwrap_or_else(|_| DEFAULT_BIND_ADDRESS.to_string()),
            pin_length: parse("PIN_LENGTH", DEFAULT_PIN_LENGTH)?,
            max_payload_bytes: parse("MAX_PAYLOAD_BYTES", DEFAULT_MAX_PAYLOAD_BYTES)?,
            stale_age: Duration::from_secs(
                60 * parse::<u64>("STALE_AGE_MINS", DEFAULT_STALE_AGE_MINS)?,
            ),
            cleanup_interval: Duration::from_secs(parse(
                "CLEANUP_INTERVAL_SECS",
                DEFAULT_CLEANUP_INTERVAL_SECS,
            )?),
            max_entries: parse("MAX_ENTRIES", DEFAULT_MAX_ENTRIES)?,
            request_timeout: Duration::from_secs(parse(
                "REQUEST_TIMEOUT_SECS",
                DEFAULT_REQUEST_TIMEOUT_SECS,
            )?),
            allowed_origins: env::var("CORS_ALLOWED_ORIGINS")
                .unwrap_or_default()
                .split(',')
                .map(str::trim)
                .filter(|origin| !origin.is_empty())
                .map(str::to_string)
                .collect(),
        };
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        let checks: [(&'static str, bool, &str); 6] = [
            (
                "PIN_LENGTH",
                (1..=PIN_LENGTH_LIMIT).contains(&self.pin_length),
                "must be between 1 and 64",
            ),
            (
                "MAX_PAYLOAD_BYTES",
                self.max_payload_bytes > 0,
                "must be greater than zero",
            ),
            (
                "STALE_AGE_MINS",
                !self.stale_age.is_zero(),
                "must be greater than zero",
            ),
            (
                "CLEANUP_INTERVAL_SECS",
                !self.cleanup_interval.is_zero(),
                "must be greater than zero",
            ),
            (
                "MAX_ENTRIES",
                self.max_entries > 0,
                "must be greater than zero",
            ),
            (
                "REQUEST_TIMEOUT_SECS",
                !self.request_timeout.is_zero(),
                "must be greater than zero",
            ),
        ];

        for (key, ok, reason) in checks {
            if !ok {
                return Err(ConfigError {
                    key,
                    reason: reason.to_string(),
                });
            }
        }
        Ok(())
    }

    /// Port the service listens on, used by the `--health-check` probe.
    pub fn port(&self) -> Option<u16> {
        self.bind_address.rsplit_once(':')?.1.parse().ok()
    }
}

fn parse<T>(key: &'static str, default: T) -> Result<T, ConfigError>
where
    T: FromStr,
    T::Err: fmt::Display,
{
    match env::var(key) {
        Err(_) => Ok(default),
        Ok(raw) if raw.trim().is_empty() => Ok(default),
        Ok(raw) => raw.trim().parse().map_err(|e: T::Err| ConfigError {
            key,
            reason: format!("{e} (got {raw:?})"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_validate() {
        Config::default()
            .validate()
            .expect("defaults must be valid");
    }

    #[test]
    fn rejects_out_of_range_values() {
        let cases = [
            Config {
                pin_length: 0,
                ..Config::default()
            },
            Config {
                pin_length: 65,
                ..Config::default()
            },
            Config {
                max_payload_bytes: 0,
                ..Config::default()
            },
            Config {
                stale_age: Duration::ZERO,
                ..Config::default()
            },
            Config {
                max_entries: 0,
                ..Config::default()
            },
        ];
        for config in cases {
            assert!(config.validate().is_err());
        }
    }

    #[test]
    fn extracts_port_from_bind_address() {
        assert_eq!(Config::default().port(), Some(8080));
        assert_eq!(
            Config {
                bind_address: "[::1]:3000".into(),
                ..Config::default()
            }
            .port(),
            Some(3000)
        );
    }
}
