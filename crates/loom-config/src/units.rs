use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ByteSize(pub u64);

impl ByteSize {
    pub const fn bytes(value: u64) -> Self {
        Self(value)
    }

    pub const fn kib(value: u64) -> Self {
        Self(value * 1024)
    }

    pub const fn mib(value: u64) -> Self {
        Self(value * 1024 * 1024)
    }

    pub const fn as_u64(self) -> u64 {
        self.0
    }

    pub fn as_usize(self) -> usize {
        usize::try_from(self.0).unwrap_or(usize::MAX)
    }
}

const BYTE_UNITS: &[(&str, u64)] = &[
    ("GiB", 1 << 30),
    ("MiB", 1 << 20),
    ("KiB", 1 << 10),
    ("GB", 1_000_000_000),
    ("MB", 1_000_000),
    ("KB", 1_000),
    ("B", 1),
];

impl FromStr for ByteSize {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        let split = trimmed
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(trimmed.len());
        let (number, unit) = trimmed.split_at(split);
        let number: u64 = number
            .parse()
            .map_err(|_| format!("invalid size {text:?}; expected a value such as 64KiB"))?;
        let unit = unit.trim();
        if unit.is_empty() {
            return Ok(Self(number));
        }
        let (_, multiplier) = BYTE_UNITS
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(unit))
            .ok_or_else(|| {
                format!("invalid size unit {unit:?}; use B, KB, MB, GB, KiB, MiB, or GiB")
            })?;
        number
            .checked_mul(*multiplier)
            .map(Self)
            .ok_or_else(|| format!("size {text:?} is too large"))
    }
}

impl fmt::Display for ByteSize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (name, multiplier) in &BYTE_UNITS[..3] {
            if self.0 >= *multiplier && self.0.is_multiple_of(*multiplier) {
                return write!(f, "{}{name}", self.0 / multiplier);
            }
        }
        write!(f, "{}B", self.0)
    }
}

impl Serialize for ByteSize {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ByteSizeVisitor;

        impl Visitor<'_> for ByteSizeVisitor {
            type Value = ByteSize;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a size such as 64KiB or a number of bytes")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(ByteSize(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u64::try_from(value)
                    .map(ByteSize)
                    .map_err(|_| E::custom("size must not be negative"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                value.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(ByteSizeVisitor)
    }
}

impl JsonSchema for ByteSize {
    fn schema_name() -> Cow<'static, str> {
        "ByteSize".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A size in bytes, either a number or a string such as 64KiB or 1MB",
            "anyOf": [
                { "type": "integer", "minimum": 0 },
                { "type": "string", "pattern": "^\\s*\\d+\\s*([KMG]i?B|B)?\\s*$" }
            ]
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HumanDuration(pub Duration);

impl HumanDuration {
    pub const fn from_secs(seconds: u64) -> Self {
        Self(Duration::from_secs(seconds))
    }

    pub const fn as_duration(self) -> Duration {
        self.0
    }
}

impl FromStr for HumanDuration {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        humantime::parse_duration(s.trim())
            .map(HumanDuration)
            .map_err(|err| format!("invalid duration {s:?}: {err}"))
    }
}

impl fmt::Display for HumanDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", humantime::format_duration(self.0))
    }
}

impl Serialize for HumanDuration {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for HumanDuration {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DurationVisitor;

        impl Visitor<'_> for DurationVisitor {
            type Value = HumanDuration;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a duration such as 30s or 10m, or a number of seconds")
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Self::Value, E> {
                Ok(HumanDuration::from_secs(value))
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Self::Value, E> {
                u64::try_from(value)
                    .map(HumanDuration::from_secs)
                    .map_err(|_| E::custom("duration must not be negative"))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Self::Value, E> {
                humantime::parse_duration(value.trim())
                    .map(HumanDuration)
                    .map_err(|err| E::custom(format!("invalid duration {value:?}: {err}")))
            }
        }

        deserializer.deserialize_any(DurationVisitor)
    }
}

impl JsonSchema for HumanDuration {
    fn schema_name() -> Cow<'static, str> {
        "Duration".into()
    }

    fn json_schema(_: &mut SchemaGenerator) -> Schema {
        json_schema!({
            "description": "A duration such as 30s, 10m, or 1h 30m, or a number of seconds",
            "anyOf": [
                { "type": "integer", "minimum": 0 },
                { "type": "string" }
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_byte_sizes() {
        assert_eq!("64KiB".parse::<ByteSize>().unwrap(), ByteSize::kib(64));
        assert_eq!("1mib".parse::<ByteSize>().unwrap(), ByteSize::mib(1));
        assert_eq!("2 KB".parse::<ByteSize>().unwrap(), ByteSize(2000));
        assert_eq!("512".parse::<ByteSize>().unwrap(), ByteSize(512));
        assert!("12XB".parse::<ByteSize>().is_err());
        assert!("KiB".parse::<ByteSize>().is_err());
    }

    #[test]
    fn formats_byte_sizes() {
        assert_eq!(ByteSize::kib(50).to_string(), "50KiB");
        assert_eq!(ByteSize::mib(1).to_string(), "1MiB");
        assert_eq!(ByteSize(1500).to_string(), "1500B");
    }

    #[test]
    fn round_trips_through_json() {
        let size: ByteSize = serde_json::from_value(serde_json::json!("1MiB")).unwrap();
        assert_eq!(serde_json::to_value(size).unwrap(), "1MiB");
        let size: ByteSize = serde_json::from_value(serde_json::json!(100)).unwrap();
        assert_eq!(size, ByteSize(100));

        let duration: HumanDuration = serde_json::from_value(serde_json::json!("10m")).unwrap();
        assert_eq!(duration, HumanDuration::from_secs(600));
        assert_eq!(serde_json::to_value(duration).unwrap(), "10m");
        let duration: HumanDuration = serde_json::from_value(serde_json::json!(5)).unwrap();
        assert_eq!(duration, HumanDuration::from_secs(5));
        assert!(serde_json::from_value::<HumanDuration>(serde_json::json!("soon")).is_err());
    }
}
