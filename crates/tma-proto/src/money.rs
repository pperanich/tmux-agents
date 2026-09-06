//! Money on the wire: a JSON number with two decimals of meaning.
//!
//! The host renders `cost_usd` with exactly two decimals so the row and the pane option can never
//! disagree, and this carries that rounding rather than a raw `f64`. It also closes a round-trip
//! hole the property tests found: `serde_json`'s float parser is within 1 ULP rather than exact, so
//! a 17-significant-digit amount comes back as a different `f64` than the one that was written.
//! Rounding on both sides makes every value this field can hold a fixed point.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// Two decimals, the unit the host publishes costs in.
fn round(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

pub(crate) fn serialize<S: Serializer>(value: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    value.map(round).serialize(s)
}

pub(crate) fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<f64>, D::Error> {
    Ok(Option::<f64>::deserialize(d)?.map(round))
}
