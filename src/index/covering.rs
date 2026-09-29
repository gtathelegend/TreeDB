use crate::engine::types::{DataType, Row, Value};
use crate::error::{Error, Result};

/// Maximum number of included (covered) non-key columns in a covering secondary index.
pub const MAX_INDEX_INCLUDE_COLUMNS: usize = 16;

/// Maximum allowable serialized size (in bytes) of an individual index entry's covered payload.
pub const MAX_COVERED_PAYLOAD_SIZE: usize = 1000;

/// Serialization format version for covered index payloads.
pub const COVERED_FORMAT_VERSION: u8 = 1;

/// Deterministic, versioned binary encoding for covered column values stored in secondary index leaf entries.
#[derive(Debug, Clone, PartialEq)]
pub struct CoveredPayload;

impl CoveredPayload {
    /// Encodes a slice of typed `Value`s into an explicit, little-endian binary payload.
    ///
    /// # Binary Layout
    /// - `0`: `format_version: u8` (currently `1`)
    /// - `1..3`: `col_count: u16` (little-endian)
    /// - `3..3 + bitmap_len`: null bitmap where bit `i` is 1 if column `i` is `Value::Null`
    /// - `3 + bitmap_len..`: sequential non-null value payloads:
    ///   - `Boolean(false)`: 0x00 (1 byte)
    ///   - `Boolean(true)`: 0x01 (1 byte)
    ///   - `Int64(i)`: 8 bytes little-endian
    ///   - `UInt64(u)`: 8 bytes little-endian
    ///   - `Float64(f)`: 8 bytes IEEE-754 bit representation little-endian
    ///   - `String(s)`: 4 bytes length `u32` le + UTF-8 payload bytes
    ///   - `Bytes(b)`: 4 bytes length `u32` le + raw bytes
    pub fn encode(values: &[Value]) -> Result<Vec<u8>> {
        if values.is_empty() {
            return Ok(Vec::new());
        }

        let col_count = values.len();
        if col_count > MAX_INDEX_INCLUDE_COLUMNS {
            return Err(Error::InvalidIndexDefinition(format!(
                "included columns count {col_count} exceeds maximum of {MAX_INDEX_INCLUDE_COLUMNS}"
            )));
        }

        let bitmap_len = col_count.div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        let mut value_payload = Vec::new();

        for (idx, val) in values.iter().enumerate() {
            if val.is_null() {
                bitmap[idx / 8] |= 1 << (idx % 8);
            } else {
                match val {
                    Value::Null => unreachable!(),
                    Value::Boolean(false) => {
                        value_payload.push(0);
                    }
                    Value::Boolean(true) => {
                        value_payload.push(1);
                    }
                    Value::Int64(i) => {
                        value_payload.extend_from_slice(&i.to_le_bytes());
                    }
                    Value::UInt64(u) => {
                        value_payload.extend_from_slice(&u.to_le_bytes());
                    }
                    Value::Float64(f) => {
                        value_payload.extend_from_slice(&f.to_bits().to_le_bytes());
                    }
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        value_payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                        value_payload.extend_from_slice(bytes);
                    }
                    Value::Bytes(b) => {
                        value_payload.extend_from_slice(&(b.len() as u32).to_le_bytes());
                        value_payload.extend_from_slice(b);
                    }
                }
            }
        }

        let total_size = 3 + bitmap_len + value_payload.len();
        if total_size > MAX_COVERED_PAYLOAD_SIZE {
            return Err(Error::ValueTooLarge {
                size: total_size,
                limit: MAX_COVERED_PAYLOAD_SIZE,
            });
        }

        let mut result = Vec::with_capacity(total_size);
        result.push(COVERED_FORMAT_VERSION);
        result.extend_from_slice(&(col_count as u16).to_le_bytes());
        result.extend_from_slice(&bitmap);
        result.extend_from_slice(&value_payload);

        Ok(result)
    }

    /// Deserializes a binary covered payload back into typed `Value`s matching `expected_types`.
    pub fn decode(bytes: &[u8], expected_types: &[DataType]) -> Result<Vec<Value>> {
        if bytes.is_empty() {
            if expected_types.is_empty() {
                return Ok(Vec::new());
            }
            return Err(Error::IndexCorruption(
                "empty covered payload for non-empty expected include columns".to_string(),
            ));
        }

        if bytes.len() < 3 {
            return Err(Error::IndexCorruption(format!(
                "covered payload length {} too short for header",
                bytes.len()
            )));
        }

        let version = bytes[0];
        if version != COVERED_FORMAT_VERSION {
            return Err(Error::TupleVersionUnsupported {
                found: version,
                supported: COVERED_FORMAT_VERSION,
            });
        }

        let col_count = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
        if col_count != expected_types.len() {
            return Err(Error::ColumnCountMismatch {
                expected: expected_types.len(),
                found: col_count,
            });
        }

        let bitmap_len = col_count.div_ceil(8);
        if bytes.len() < 3 + bitmap_len {
            return Err(Error::IndexCorruption(format!(
                "covered payload truncated before null bitmap (len: {}, expected at least: {})",
                bytes.len(),
                3 + bitmap_len
            )));
        }

        let bitmap = &bytes[3..3 + bitmap_len];
        let mut offset = 3 + bitmap_len;
        let mut values = Vec::with_capacity(col_count);

        for (idx, &dt) in expected_types.iter().enumerate() {
            let is_null = (bitmap[idx / 8] & (1 << (idx % 8))) != 0;
            if is_null {
                values.push(Value::Null);
            } else {
                match dt {
                    DataType::Null => {
                        values.push(Value::Null);
                    }
                    DataType::Boolean => {
                        if offset + 1 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated boolean in covered payload".to_string(),
                            ));
                        }
                        let b = bytes[offset];
                        offset += 1;
                        match b {
                            0 => values.push(Value::Boolean(false)),
                            1 => values.push(Value::Boolean(true)),
                            other => {
                                return Err(Error::IndexCorruption(format!(
                                    "invalid boolean byte {other} in covered payload"
                                )));
                            }
                        }
                    }
                    DataType::Int64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated Int64 in covered payload".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        values.push(Value::Int64(i64::from_le_bytes(b)));
                    }
                    DataType::UInt64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated UInt64 in covered payload".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        values.push(Value::UInt64(u64::from_le_bytes(b)));
                    }
                    DataType::Float64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated Float64 in covered payload".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        values.push(Value::Float64(f64::from_bits(u64::from_le_bytes(b))));
                    }
                    DataType::String => {
                        if offset + 4 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated string length in covered payload".to_string(),
                            ));
                        }
                        let len = u32::from_le_bytes([
                            bytes[offset],
                            bytes[offset + 1],
                            bytes[offset + 2],
                            bytes[offset + 3],
                        ]) as usize;
                        offset += 4;
                        if offset + len > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated string payload in covered payload".to_string(),
                            ));
                        }
                        let s =
                            std::str::from_utf8(&bytes[offset..offset + len]).map_err(|_| {
                                Error::IndexCorruption(
                                    "invalid UTF-8 string in covered payload".to_string(),
                                )
                            })?;
                        offset += len;
                        values.push(Value::String(s.to_string()));
                    }
                    DataType::Bytes => {
                        if offset + 4 > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated bytes length in covered payload".to_string(),
                            ));
                        }
                        let len = u32::from_le_bytes([
                            bytes[offset],
                            bytes[offset + 1],
                            bytes[offset + 2],
                            bytes[offset + 3],
                        ]) as usize;
                        offset += 4;
                        if offset + len > bytes.len() {
                            return Err(Error::IndexCorruption(
                                "truncated bytes payload in covered payload".to_string(),
                            ));
                        }
                        let b = bytes[offset..offset + len].to_vec();
                        offset += len;
                        values.push(Value::Bytes(b));
                    }
                }
            }
        }

        Ok(values)
    }

    /// Extracts covered values from a `Row` given the included column indices.
    pub fn extract_from_row(row: &Row, include_column_indices: &[usize]) -> Vec<Value> {
        include_column_indices
            .iter()
            .map(|&idx| row.values[idx].clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_covered_payload_roundtrip() {
        let values = vec![
            Value::String("Alice".to_string()),
            Value::Int64(100_000),
            Value::Null,
            Value::Boolean(true),
            Value::Float64(98.5),
            Value::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ];
        let types = vec![
            DataType::String,
            DataType::Int64,
            DataType::Int64,
            DataType::Boolean,
            DataType::Float64,
            DataType::Bytes,
        ];

        let encoded = CoveredPayload::encode(&values).unwrap();
        let decoded = CoveredPayload::decode(&encoded, &types).unwrap();
        assert_eq!(values, decoded);
    }

    #[test]
    fn test_empty_covered_payload() {
        let values = vec![];
        let types = vec![];
        let encoded = CoveredPayload::encode(&values).unwrap();
        assert!(encoded.is_empty());
        let decoded = CoveredPayload::decode(&encoded, &types).unwrap();
        assert!(decoded.is_empty());
    }

    #[test]
    fn test_oversized_covered_payload() {
        let large_string = "a".repeat(MAX_COVERED_PAYLOAD_SIZE + 10);
        let values = vec![Value::String(large_string)];
        let res = CoveredPayload::encode(&values);
        assert!(matches!(res, Err(Error::ValueTooLarge { .. })));
    }
}
