use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

pub fn encode_key(key: &[u8; 32]) -> String {
    URL_SAFE_NO_PAD.encode(key)
}

pub fn decode_key(text: &str) -> Option<[u8; 32]> {
    URL_SAFE_NO_PAD.decode(text.trim()).ok()?.try_into().ok()
}

pub(crate) mod key_b64 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(key: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::encode_key(key))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(d)?;
        super::decode_key(&text).ok_or_else(|| serde::de::Error::custom("invalid base64 key"))
    }
}
