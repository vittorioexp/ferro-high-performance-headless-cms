use serde_json::Value;
use sha2::{Digest, Sha256};

/// Canonical JSON: object keys sorted, no whitespace. Independent of the
/// `preserve_order` feature of serde_json.
pub fn canonical_json(v: &Value) -> String {
    let mut out = String::new();
    write_canonical(v, &mut out);
    out
}

fn write_canonical(v: &Value, out: &mut String) {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String((*k).clone()).to_string());
                out.push(':');
                write_canonical(&map[k.as_str()], out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

pub fn sha256_hex(data: &[u8]) -> String {
    hex::encode(Sha256::digest(data))
}

pub fn hash_json(v: &Value) -> String {
    sha256_hex(canonical_json(v).as_bytes())
}

/// Short deterministic hash used for constraint/index names.
pub fn short_hash(s: &str) -> String {
    sha256_hex(s.as_bytes())[..16].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_is_key_order_independent() {
        let a = json!({"b": 1, "a": {"y": [1, 2], "x": null}});
        let b = json!({"a": {"x": null, "y": [1, 2]}, "b": 1});
        assert_eq!(canonical_json(&a), canonical_json(&b));
        assert_eq!(canonical_json(&a), r#"{"a":{"x":null,"y":[1,2]},"b":1}"#);
    }
}
