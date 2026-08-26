use rand::{RngExt, rng};
use serde::Serialize;
use std::fmt;

/// Crockford base32: 32 symbols so `random_range` is unbiased, minus the
/// `O`/`0` and `I`/`L`/`1` pairs that get misread when a PIN is spoken aloud.
pub const ALPHABET: &[u8] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub const MAX_NAMESPACE_LEN: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invalid {
    Namespace,
    Pin,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Namespace(String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct Pin(String);

impl Namespace {
    pub fn parse(raw: &str) -> Result<Self, Invalid> {
        let valid = (1..=MAX_NAMESPACE_LEN).contains(&raw.len())
            && raw
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');

        valid
            .then(|| Self(raw.to_string()))
            .ok_or(Invalid::Namespace)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Pin {
    pub fn parse(raw: &str, length: usize) -> Result<Self, Invalid> {
        // Normalise case at the single boundary where PINs enter the system, so
        // a lowercase PIN from a URL bar resolves to the slot it was issued for.
        let normalised = raw.to_ascii_uppercase();

        let valid = normalised.len() == length && normalised.bytes().all(|b| ALPHABET.contains(&b));

        valid.then_some(Self(normalised)).ok_or(Invalid::Pin)
    }

    pub fn generate(length: usize) -> Self {
        let mut rng = rng();
        Self(
            (0..length)
                .map(|_| ALPHABET[rng.random_range(..ALPHABET.len())] as char)
                .collect(),
        )
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Namespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Display for Pin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn alphabet_is_a_power_of_two_and_free_of_confusables() {
        assert_eq!(ALPHABET.len(), 32, "unbiased sampling needs a power of two");
        assert_eq!(
            ALPHABET.iter().collect::<HashSet<_>>().len(),
            ALPHABET.len(),
            "duplicate symbols would skew the distribution"
        );
        for confusable in b"OILU" {
            assert!(!ALPHABET.contains(confusable));
        }
    }

    #[test]
    fn generated_pins_are_valid_and_the_right_length() {
        for length in [1, 4, 8, 32] {
            let pin = Pin::generate(length);
            assert_eq!(pin.as_str().len(), length);
            assert_eq!(Pin::parse(pin.as_str(), length), Ok(pin));
        }
    }

    #[test]
    fn generation_covers_the_whole_alphabet() {
        // A uniform draw over 32 symbols across 20k characters hits every symbol
        // with overwhelming probability; a folded or truncated alphabet would not.
        let seen: HashSet<u8> = (0..5000)
            .flat_map(|_| Pin::generate(4).as_str().bytes().collect::<Vec<_>>())
            .collect();
        assert_eq!(
            seen.len(),
            ALPHABET.len(),
            "sampling missed symbols: {seen:?}"
        );
    }

    #[test]
    fn namespace_validation() {
        for ok in ["a", "tenant_1", "TENANT-eu", &"x".repeat(MAX_NAMESPACE_LEN)] {
            assert!(Namespace::parse(ok).is_ok(), "{ok:?} should be accepted");
        }
        for bad in [
            "",
            "tenant:eu",
            "has.dot",
            "has/slash",
            "日本語",
            &"x".repeat(MAX_NAMESPACE_LEN + 1),
        ] {
            assert_eq!(Namespace::parse(bad), Err(Invalid::Namespace), "{bad:?}");
        }
    }

    #[test]
    fn pin_validation_normalises_case_and_rejects_bad_shapes() {
        assert_eq!(Pin::parse("abcd", 4).unwrap().as_str(), "ABCD");
        for bad in ["ABC", "ABCDE", "AB!D", "ABOD", "ABID", ""] {
            assert_eq!(Pin::parse(bad, 4), Err(Invalid::Pin), "{bad:?}");
        }
    }
}
