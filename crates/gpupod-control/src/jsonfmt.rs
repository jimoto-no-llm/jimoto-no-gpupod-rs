//! Django の JsonResponse と同じ JSON 出力（区切りは ", " / ": "、ASCII は \\uXXXX エスケープ）。

use serde_json::Value;

/// Django の json.dumps 相当（ensure_ascii=True, default separators）。
pub fn django_json(value: &Value) -> String {
    use serde_json::ser::Formatter;
    struct DjangoFormatter;

    impl Formatter for DjangoFormatter {
        fn begin_array_value<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if !first {
                writer.write_all(b", ")?;
            }
            Ok(())
        }

        fn begin_object_key<W>(&mut self, writer: &mut W, first: bool) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            if !first {
                writer.write_all(b", ")?;
            }
            Ok(())
        }

        fn begin_object_value<W>(&mut self, writer: &mut W) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            writer.write_all(b": ")
        }

        fn write_string_fragment<W>(&mut self, writer: &mut W, fragment: &str) -> std::io::Result<()>
        where
            W: ?Sized + std::io::Write,
        {
            for ch in fragment.chars() {
                let code = ch as u32;
                if code <= 0x7e {
                    writer.write_all(&(code as u8).to_be_bytes())?;
                } else if code <= 0xffff {
                    writer.write_all(format!("\\u{:04x}", code).as_bytes())?;
                } else {
                    let adjusted = code - 0x1_0000;
                    let high = 0xd800 + (adjusted >> 10);
                    let low = 0xdc00 + (adjusted & 0x3ff);
                    writer.write_all(format!("\\u{high:04x}\\u{low:04x}").as_bytes())?;
                }
            }
            Ok(())
        }
    }

    let mut buffer = Vec::new();
    let mut serializer = serde_json::Serializer::with_formatter(&mut buffer, DjangoFormatter);
    match value.serialize(&mut serializer) {
        Ok(()) => String::from_utf8(buffer).unwrap_or_default(),
        Err(_) => value.to_string(),
    }
}

use serde::Serialize;

/// JsonResponse 1 行（{"key": "value"}）。
pub fn json_object(pairs: &[(&str, Value)]) -> String {
    let map: serde_json::Map<String, Value> = pairs
        .iter()
        .map(|(key, value)| ((*key).to_string(), value.clone()))
        .collect();
    django_json(&Value::Object(map))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn django_separators_and_ascii_escape() {
        assert_eq!(django_json(&json!({"error": "Invalid"})), r#"{"error": "Invalid"}"#);
        assert_eq!(django_json(&json!({"a": 1, "b": [1, 2]})), r#"{"a": 1, "b": [1, 2]}"#);
        assert_eq!(django_json(&json!({"error": "貸出中"})), r#"{"error": "\u8cb8\u51fa\u4e2d"}"#);
    }
}
