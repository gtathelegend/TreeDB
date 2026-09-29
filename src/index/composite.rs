use crate::engine::types::{Row, Value};
use crate::error::{Error, Result};
use crate::index::secondary::IndexKey;

/// Maximum number of columns supported in a composite secondary index.
pub const MAX_INDEX_COLUMNS: usize = 8;

/// Canonical, order-preserving encoded composite key for multi-column secondary B+ Tree indexing.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CompositeIndexKey {
    /// Ordered list of scalar component keys.
    pub components: Vec<IndexKey>,
}

impl CompositeIndexKey {
    /// Creates a new `CompositeIndexKey` from constituent scalar index keys.
    pub fn new(components: Vec<IndexKey>) -> Self {
        Self { components }
    }

    /// Creates a new `CompositeIndexKey` by encoding a list of typed values.
    pub fn from_values(values: &[Value]) -> Self {
        let components = values.iter().map(IndexKey::from_value).collect();
        Self { components }
    }

    /// Creates a new `CompositeIndexKey` by extracting column values from a `Row`.
    pub fn from_row_columns(row: &Row, column_indices: &[usize]) -> Self {
        let components = column_indices
            .iter()
            .map(|&idx| IndexKey::from_value(&row.values[idx]))
            .collect();
        Self { components }
    }

    /// Encodes this composite key into an order-preserving byte vector.
    ///
    /// # Encoding Rules
    /// - If `components.len() == 1`: returns `components[0].bytes` as-is, preserving 100% byte-for-byte
    ///   compatibility with existing single-column secondary index formats.
    /// - If `components.len() > 1`: encodes each component in order using `encode_component_into`.
    ///   - Fixed-length components (`Null`, `Boolean`, `Int64`, `UInt64`, `Float64`): written directly.
    ///   - Variable-length components (`String`, `Bytes`): tag `6` or `7`, followed by the payload where
    ///     `0x00` is escaped as `[0x00, 0xFF]`, and terminated with `[0x00, 0x00]`.
    ///
    ///   This guarantees that lexicographic byte comparison strictly matches lexicographic tuple comparison.
    pub fn encode(&self) -> Vec<u8> {
        if self.components.is_empty() {
            return Vec::new();
        }
        if self.components.len() == 1 {
            return self.components[0].bytes.clone();
        }
        let mut buf = Vec::new();
        for comp in &self.components {
            Self::encode_component_into(&mut buf, comp);
        }
        buf
    }

    /// Encodes an individual scalar `IndexKey` component into `buf` using the order-preserving framing rule.
    pub fn encode_component_into(buf: &mut Vec<u8>, key: &IndexKey) {
        if key.bytes.is_empty() {
            return;
        }
        let tag = key.bytes[0];
        match tag {
            // Null (0), Boolean(false) (1), Boolean(true) (2): 1 byte total
            0..=2 => {
                buf.push(tag);
            }
            // Int64 (3), UInt64 (4), Float64 (5): tag + 8 bytes = 9 bytes total
            3..=5 => {
                buf.extend_from_slice(&key.bytes);
            }
            // String (6), Bytes (7): variable-length byte string
            6..=7 => {
                buf.push(tag);
                for &b in &key.bytes[1..] {
                    if b == 0x00 {
                        buf.push(0x00);
                        buf.push(0xFF);
                    } else {
                        buf.push(b);
                    }
                }
                // Terminate variable-length component
                buf.push(0x00);
                buf.push(0x00);
            }
            // Other tags (defensive fallback)
            _ => {
                buf.extend_from_slice(&key.bytes);
            }
        }
    }

    /// Encodes a prefix of values for an index with `total_columns`.
    pub fn encode_prefix_values(prefix_values: &[Value], total_columns: usize) -> Vec<u8> {
        if prefix_values.is_empty() {
            return Vec::new();
        }
        if total_columns == 1 {
            return IndexKey::from_value(&prefix_values[0]).bytes;
        }
        let mut buf = Vec::new();
        for val in prefix_values {
            let k = IndexKey::from_value(val);
            Self::encode_component_into(&mut buf, &k);
        }
        buf
    }

    /// Converts this `CompositeIndexKey` into a standard `IndexKey` for storage or search in `SecondaryIndex`.
    pub fn to_index_key(&self, total_columns: usize) -> IndexKey {
        if total_columns == 1 && !self.components.is_empty() {
            self.components[0].clone()
        } else {
            IndexKey::new(self.encode())
        }
    }

    /// Computes the lexicographic successor of a byte prefix for upper-bounded range scans.
    ///
    /// Scans backwards from the end of `prefix`: increments the first byte strictly less than `0xFF`
    /// and truncates all trailing `0xFF` bytes. Returns `None` if all bytes in `prefix` are `0xFF`.
    pub fn prefix_successor(prefix: &[u8]) -> Option<Vec<u8>> {
        if prefix.is_empty() {
            return None;
        }
        let mut succ = prefix.to_vec();
        for i in (0..succ.len()).rev() {
            if succ[i] < 0xFF {
                succ[i] += 1;
                succ.truncate(i + 1);
                return Some(succ);
            }
        }
        None
    }

    /// Decodes an order-preserving byte slice back into constituent `IndexKey` components.
    pub fn decode(bytes: &[u8], total_columns: usize) -> Result<Self> {
        if total_columns == 1 {
            return Ok(Self::new(vec![IndexKey::new(bytes.to_vec())]));
        }

        let mut components = Vec::new();
        let mut offset = 0;

        while offset < bytes.len() && components.len() < total_columns {
            let tag = bytes[offset];
            match tag {
                0..=2 => {
                    components.push(IndexKey::new(vec![tag]));
                    offset += 1;
                }
                3..=5 => {
                    if offset + 9 > bytes.len() {
                        return Err(Error::IndexCorruption(
                            "unexpected end of bytes reading fixed-length composite key component"
                                .to_string(),
                        ));
                    }
                    components.push(IndexKey::new(bytes[offset..offset + 9].to_vec()));
                    offset += 9;
                }
                6..=7 => {
                    let mut payload = vec![tag];
                    let mut cur = offset + 1;
                    let mut terminated = false;

                    while cur < bytes.len() {
                        if bytes[cur] == 0x00 {
                            if cur + 1 < bytes.len() {
                                if bytes[cur + 1] == 0xFF {
                                    payload.push(0x00);
                                    cur += 2;
                                    continue;
                                } else if bytes[cur + 1] == 0x00 {
                                    cur += 2;
                                    terminated = true;
                                    break;
                                }
                            }
                            return Err(Error::IndexCorruption(
                                "corrupt escaped 0x00 in variable-length composite component"
                                    .to_string(),
                            ));
                        } else {
                            payload.push(bytes[cur]);
                            cur += 1;
                        }
                    }

                    if !terminated {
                        return Err(Error::IndexCorruption(
                            "unterminated variable-length composite component".to_string(),
                        ));
                    }

                    components.push(IndexKey::new(payload));
                    offset = cur;
                }
                other => {
                    return Err(Error::IndexCorruption(format!(
                        "unknown tag {other} in composite index key"
                    )));
                }
            }
        }

        Ok(Self::new(components))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_single_column_preserves_scalar_index_key() {
        let val = Value::String("hello".to_string());
        let k = IndexKey::from_value(&val);
        let comp = CompositeIndexKey::new(vec![k.clone()]);
        assert_eq!(comp.to_index_key(1), k);
    }

    #[test]
    fn test_lexicographic_ordering() {
        // (1, "a") < (1, "b")
        let t1 = CompositeIndexKey::from_values(&[Value::Int64(1), Value::String("a".into())]);
        let t2 = CompositeIndexKey::from_values(&[Value::Int64(1), Value::String("b".into())]);
        assert!(t1.encode() < t2.encode());

        // (1, "z") < (2, "a")
        let t3 = CompositeIndexKey::from_values(&[Value::Int64(1), Value::String("z".into())]);
        let t4 = CompositeIndexKey::from_values(&[Value::Int64(2), Value::String("a".into())]);
        assert!(t3.encode() < t4.encode());

        // ("a", 100) < ("ab", 1)
        let t5 = CompositeIndexKey::from_values(&[Value::String("a".into()), Value::Int64(100)]);
        let t6 = CompositeIndexKey::from_values(&[Value::String("ab".into()), Value::Int64(1)]);
        assert!(t5.encode() < t6.encode());

        // ("a\x00", 1) > ("a", 100)
        let t7 = CompositeIndexKey::from_values(&[Value::String("a\0".into()), Value::Int64(1)]);
        assert!(t5.encode() < t7.encode());
    }

    #[test]
    fn test_prefix_successor() {
        let p1 = vec![1, 2, 3];
        assert_eq!(
            CompositeIndexKey::prefix_successor(&p1),
            Some(vec![1, 2, 4])
        );

        let p2 = vec![1, 2, 0xFF];
        assert_eq!(CompositeIndexKey::prefix_successor(&p2), Some(vec![1, 3]));

        let p3 = vec![0xFF, 0xFF];
        assert_eq!(CompositeIndexKey::prefix_successor(&p3), None);
    }

    #[test]
    fn test_roundtrip_decoding() {
        let values = vec![
            Value::Int64(-42),
            Value::String("test string with \0 null".to_string()),
            Value::UInt64(999),
            Value::Null,
            Value::Boolean(true),
        ];
        let comp = CompositeIndexKey::from_values(&values);
        let encoded = comp.encode();
        let decoded = CompositeIndexKey::decode(&encoded, values.len()).unwrap();
        assert_eq!(comp, decoded);
    }
}
