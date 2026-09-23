use std::collections::HashSet;
use std::fmt;

use crate::error::{Error, Result};
use crate::storage::page::Key;

/// Current binary serialization format version for tuples.
pub const TUPLE_FORMAT_VERSION: u8 = 1;

/// Maximum allowable length for a column name in bytes.
pub const MAX_COLUMN_NAME_LEN: usize = 64;

/// Supported primitive data types for relational table columns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DataType {
    /// Null/unknown sentinel type.
    Null = 0,
    /// 1-byte boolean (true/false).
    Boolean = 1,
    /// 8-byte signed 64-bit integer.
    Int64 = 2,
    /// 8-byte unsigned 64-bit integer.
    UInt64 = 3,
    /// 8-byte IEEE-754 double precision floating point number.
    Float64 = 4,
    /// Variable-length UTF-8 text string.
    String = 5,
    /// Variable-length raw binary blob.
    Bytes = 6,
}

impl DataType {
    /// Converts a raw discriminant byte into a `DataType`.
    pub fn from_u8(value: u8) -> Result<Self> {
        match value {
            0 => Ok(DataType::Null),
            1 => Ok(DataType::Boolean),
            2 => Ok(DataType::Int64),
            3 => Ok(DataType::UInt64),
            4 => Ok(DataType::Float64),
            5 => Ok(DataType::String),
            6 => Ok(DataType::Bytes),
            _ => Err(Error::SerializationFailure(format!(
                "invalid data type tag: {value}"
            ))),
        }
    }

    /// Converts this `DataType` into its persistent byte representation.
    pub const fn to_u8(self) -> u8 {
        self as u8
    }
}

impl fmt::Display for DataType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DataType::Null => write!(f, "Null"),
            DataType::Boolean => write!(f, "Boolean"),
            DataType::Int64 => write!(f, "Int64"),
            DataType::UInt64 => write!(f, "UInt64"),
            DataType::Float64 => write!(f, "Float64"),
            DataType::String => write!(f, "String"),
            DataType::Bytes => write!(f, "Bytes"),
        }
    }
}

/// Dynamically typed scalar value stored in a tuple column.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// Represents a missing or null value.
    Null,
    /// Boolean value (`true` / `false`).
    Boolean(bool),
    /// Signed 64-bit integer (`i64`).
    Int64(i64),
    /// Unsigned 64-bit integer (`u64`).
    UInt64(u64),
    /// Double precision 64-bit float (`f64`).
    Float64(f64),
    /// Variable-length UTF-8 string.
    String(String),
    /// Variable-length raw binary bytes.
    Bytes(Vec<u8>),
}

impl Value {
    /// Returns the corresponding `DataType` for this value, or `DataType::Null` if null.
    pub fn data_type(&self) -> DataType {
        match self {
            Value::Null => DataType::Null,
            Value::Boolean(_) => DataType::Boolean,
            Value::Int64(_) => DataType::Int64,
            Value::UInt64(_) => DataType::UInt64,
            Value::Float64(_) => DataType::Float64,
            Value::String(_) => DataType::String,
            Value::Bytes(_) => DataType::Bytes,
        }
    }

    /// Returns `true` if this value is `Value::Null`.
    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Encodes this primary key value into a canonical B+ Tree `Key` (`u64`).
    ///
    /// - `UInt64(u)` maps directly to `u as u64`.
    /// - `Int64(i)` maps to `(i as u64) ^ (1 << 63)`, preserving signed ordering monotonically.
    pub fn encode_to_canonical_key(&self) -> Result<Key> {
        match self {
            Value::UInt64(u) => Ok(*u),
            Value::Int64(i) => {
                // Flip the sign bit to preserve signed ordering in unsigned B+ Tree
                Ok((*i as u64) ^ (1u64 << 63))
            }
            other => Err(Error::InvalidPrimaryKey(format!(
                "unsupported primary key value type: {:?}",
                other.data_type()
            ))),
        }
    }

    /// Decodes a canonical B+ Tree `Key` (`u64`) back into a typed `Value`.
    pub fn decode_from_canonical_key(key: Key, data_type: DataType) -> Result<Self> {
        match data_type {
            DataType::UInt64 => Ok(Value::UInt64(key)),
            DataType::Int64 => {
                let original = (key ^ (1u64 << 63)) as i64;
                Ok(Value::Int64(original))
            }
            other => Err(Error::InvalidPrimaryKey(format!(
                "unsupported primary key data type: {other:?}"
            ))),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "NULL"),
            Value::Boolean(b) => write!(f, "{b}"),
            Value::Int64(i) => write!(f, "{i}"),
            Value::UInt64(u) => write!(f, "{u}"),
            Value::Float64(v) => write!(f, "{v}"),
            Value::String(s) => write!(f, "\"{s}\""),
            Value::Bytes(b) => write!(f, "<{} bytes>", b.len()),
        }
    }
}

/// Description of an individual column in a table schema.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    /// Column identifier name.
    pub name: String,
    /// Logical data type of the column.
    pub data_type: DataType,
    /// Whether this column permits `NULL` values.
    pub nullable: bool,
}

impl Column {
    /// Creates a new `Column` definition.
    pub fn new(name: impl Into<String>, data_type: DataType, nullable: bool) -> Self {
        Self {
            name: name.into(),
            data_type,
            nullable,
        }
    }

    /// Validates column definition constraints.
    pub fn validate(&self) -> Result<()> {
        if self.name.is_empty() {
            return Err(Error::TableSchemaMismatch(
                "column name cannot be empty".to_string(),
            ));
        }
        if self.name.len() > MAX_COLUMN_NAME_LEN {
            return Err(Error::TableSchemaMismatch(format!(
                "column name '{}' length {} exceeds limit of {}",
                self.name,
                self.name.len(),
                MAX_COLUMN_NAME_LEN
            )));
        }
        if self.data_type == DataType::Null {
            return Err(Error::TableSchemaMismatch(format!(
                "column '{}' cannot have type DataType::Null",
                self.name
            )));
        }
        Ok(())
    }
}

/// Schema definition for a relational table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schema {
    /// Ordered list of columns defining the table structure.
    pub columns: Vec<Column>,
    /// Index of the single primary key column within `columns`.
    pub primary_key_idx: usize,
}

impl Schema {
    /// Creates and validates a new `Schema` definition for a persistent table.
    pub fn new(columns: Vec<Column>, primary_key_idx: usize) -> Result<Self> {
        let schema = Self {
            columns,
            primary_key_idx,
        };
        schema.validate()?;
        Ok(schema)
    }

    /// Creates a `Schema` representation for a query result or projection.
    pub fn new_projected(columns: Vec<Column>) -> Self {
        Self {
            columns,
            primary_key_idx: 0,
        }
    }

    /// Validates structural invariants of the schema.
    pub fn validate(&self) -> Result<()> {
        if self.columns.is_empty() {
            return Err(Error::TableSchemaMismatch(
                "schema must contain at least one column".to_string(),
            ));
        }

        if self.primary_key_idx >= self.columns.len() {
            return Err(Error::TableSchemaMismatch(format!(
                "primary key index {} is out of bounds (column count: {})",
                self.primary_key_idx,
                self.columns.len()
            )));
        }

        let mut seen_names = HashSet::new();
        for col in &self.columns {
            col.validate()?;
            if !seen_names.insert(col.name.clone()) {
                return Err(Error::TableSchemaMismatch(format!(
                    "duplicate column name '{}' in schema",
                    col.name
                )));
            }
        }

        let pk_col = &self.columns[self.primary_key_idx];
        if pk_col.nullable {
            return Err(Error::TableSchemaMismatch(format!(
                "primary key column '{}' cannot be nullable",
                pk_col.name
            )));
        }

        if pk_col.data_type != DataType::UInt64 && pk_col.data_type != DataType::Int64 {
            return Err(Error::TableSchemaMismatch(format!(
                "primary key column '{}' has unsupported type {:?} (must be UInt64 or Int64)",
                pk_col.name, pk_col.data_type
            )));
        }

        Ok(())
    }

    /// Returns the primary key column definition.
    pub fn primary_key_column(&self) -> &Column {
        &self.columns[self.primary_key_idx]
    }

    /// Finds the index of a column by name.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name == name)
    }

    /// Finds a column definition by name.
    pub fn get_column(&self, name: &str) -> Option<&Column> {
        self.columns.iter().find(|c| c.name == name)
    }

    /// Validates a `Row` against this schema.
    pub fn validate_row(&self, row: &Row) -> Result<()> {
        if row.values.len() != self.columns.len() {
            return Err(Error::ColumnCountMismatch {
                expected: self.columns.len(),
                found: row.values.len(),
            });
        }

        for (idx, (col, val)) in self.columns.iter().zip(row.values.iter()).enumerate() {
            match val {
                Value::Null => {
                    if !col.nullable {
                        return Err(Error::NullConstraintViolation {
                            column: col.name.clone(),
                        });
                    }
                }
                Value::Boolean(_) => {
                    if col.data_type != DataType::Boolean {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "Boolean".to_string(),
                        });
                    }
                }
                Value::Int64(_) => {
                    if col.data_type != DataType::Int64 {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "Int64".to_string(),
                        });
                    }
                }
                Value::UInt64(_) => {
                    if col.data_type != DataType::UInt64 {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "UInt64".to_string(),
                        });
                    }
                }
                Value::Float64(_) => {
                    if col.data_type != DataType::Float64 {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "Float64".to_string(),
                        });
                    }
                }
                Value::String(_) => {
                    if col.data_type != DataType::String {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "String".to_string(),
                        });
                    }
                }
                Value::Bytes(_) => {
                    if col.data_type != DataType::Bytes {
                        return Err(Error::TypeMismatch {
                            column: col.name.clone(),
                            expected: col.data_type.to_string(),
                            found: "Bytes".to_string(),
                        });
                    }
                }
            }

            if idx == self.primary_key_idx && val.is_null() {
                return Err(Error::NullConstraintViolation {
                    column: col.name.clone(),
                });
            }
        }

        Ok(())
    }

    /// Extracts the canonical B+ Tree `Key` (`u64`) from the primary key column of a `Row`.
    pub fn extract_primary_key(&self, row: &Row) -> Result<Key> {
        if self.primary_key_idx >= row.values.len() {
            return Err(Error::ColumnCountMismatch {
                expected: self.columns.len(),
                found: row.values.len(),
            });
        }
        let pk_val = &row.values[self.primary_key_idx];
        pk_val.encode_to_canonical_key()
    }

    /// Serializes a `Row` into a deterministic binary tuple format.
    ///
    /// # Binary Layout
    /// - `0`: `format_version: u8` (`1`)
    /// - `1..3`: `column_count: u16` (LE)
    /// - `3..3+bitmap_len`: Null bitmap (ceil(column_count / 8) bytes, 1 = NULL, 0 = Value present)
    /// - `3+bitmap_len..`: Packed column payloads in sequential order:
    ///   - `Boolean`: 1 byte (`0` or `1`)
    ///   - `Int64`: 8 bytes (`i64` LE)
    ///   - `UInt64`: 8 bytes (`u64` LE)
    ///   - `Float64`: 8 bytes (`f64` bits LE)
    ///   - `String`: 4 bytes length (`u32` LE) + UTF-8 bytes
    ///   - `Bytes`: 4 bytes length (`u32` LE) + raw bytes
    pub fn encode_tuple(&self, row: &Row) -> Result<Vec<u8>> {
        self.validate_row(row)?;

        let col_count = self.columns.len();
        let bitmap_len = col_count.div_ceil(8);
        let mut bitmap = vec![0u8; bitmap_len];
        let mut payload = Vec::new();

        for (idx, val) in row.values.iter().enumerate() {
            if val.is_null() {
                bitmap[idx / 8] |= 1 << (idx % 8);
            } else {
                match val {
                    Value::Null => unreachable!(),
                    Value::Boolean(b) => {
                        payload.push(if *b { 1 } else { 0 });
                    }
                    Value::Int64(i) => {
                        payload.extend_from_slice(&i.to_le_bytes());
                    }
                    Value::UInt64(u) => {
                        payload.extend_from_slice(&u.to_le_bytes());
                    }
                    Value::Float64(f) => {
                        payload.extend_from_slice(&f.to_bits().to_le_bytes());
                    }
                    Value::String(s) => {
                        let bytes = s.as_bytes();
                        payload.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
                        payload.extend_from_slice(bytes);
                    }
                    Value::Bytes(b) => {
                        payload.extend_from_slice(&(b.len() as u32).to_le_bytes());
                        payload.extend_from_slice(b);
                    }
                }
            }
        }

        let mut result = Vec::with_capacity(3 + bitmap_len + payload.len());
        result.push(TUPLE_FORMAT_VERSION);
        result.extend_from_slice(&(col_count as u16).to_le_bytes());
        result.extend_from_slice(&bitmap);
        result.extend_from_slice(&payload);

        Ok(result)
    }

    /// Deserializes a binary tuple slice back into a typed `Row`.
    pub fn decode_tuple(&self, bytes: &[u8]) -> Result<Row> {
        if bytes.len() < 3 {
            return Err(Error::TupleCorruption(format!(
                "tuple length {} too short for header",
                bytes.len()
            )));
        }

        let version = bytes[0];
        if version != TUPLE_FORMAT_VERSION {
            return Err(Error::TupleVersionUnsupported {
                found: version,
                supported: TUPLE_FORMAT_VERSION,
            });
        }

        let col_count = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
        if col_count != self.columns.len() {
            return Err(Error::ColumnCountMismatch {
                expected: self.columns.len(),
                found: col_count,
            });
        }

        let bitmap_len = col_count.div_ceil(8);
        if bytes.len() < 3 + bitmap_len {
            return Err(Error::TupleCorruption(format!(
                "tuple truncated before null bitmap (len: {}, expected at least: {})",
                bytes.len(),
                3 + bitmap_len
            )));
        }

        let bitmap = &bytes[3..3 + bitmap_len];
        let mut offset = 3 + bitmap_len;
        let mut values = Vec::with_capacity(col_count);

        for (idx, col) in self.columns.iter().enumerate() {
            let is_null = (bitmap[idx / 8] & (1 << (idx % 8))) != 0;

            if is_null {
                if !col.nullable {
                    return Err(Error::NullConstraintViolation {
                        column: col.name.clone(),
                    });
                }
                values.push(Value::Null);
            } else {
                match col.data_type {
                    DataType::Null => {
                        return Err(Error::TupleCorruption(
                            "unexpected DataType::Null in column schema".to_string(),
                        ));
                    }
                    DataType::Boolean => {
                        if offset + 1 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated boolean value".to_string(),
                            ));
                        }
                        let b = bytes[offset];
                        offset += 1;
                        match b {
                            0 => values.push(Value::Boolean(false)),
                            1 => values.push(Value::Boolean(true)),
                            other => {
                                return Err(Error::TupleCorruption(format!(
                                    "invalid boolean byte {other} in tuple"
                                )));
                            }
                        }
                    }
                    DataType::Int64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated Int64 value".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        values.push(Value::Int64(i64::from_le_bytes(b)));
                    }
                    DataType::UInt64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated UInt64 value".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        values.push(Value::UInt64(u64::from_le_bytes(b)));
                    }
                    DataType::Float64 => {
                        if offset + 8 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated Float64 value".to_string(),
                            ));
                        }
                        let mut b = [0u8; 8];
                        b.copy_from_slice(&bytes[offset..offset + 8]);
                        offset += 8;
                        let u = u64::from_le_bytes(b);
                        values.push(Value::Float64(f64::from_bits(u)));
                    }
                    DataType::String => {
                        if offset + 4 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated string length".to_string(),
                            ));
                        }
                        let str_len = u32::from_le_bytes([
                            bytes[offset],
                            bytes[offset + 1],
                            bytes[offset + 2],
                            bytes[offset + 3],
                        ]) as usize;
                        offset += 4;

                        if offset + str_len > bytes.len() {
                            return Err(Error::TupleCorruption(format!(
                                "string length {str_len} exceeds tuple bounds (remaining: {})",
                                bytes.len() - offset
                            )));
                        }

                        let s = std::str::from_utf8(&bytes[offset..offset + str_len])
                            .map_err(|_| {
                                Error::TupleCorruption("invalid UTF-8 in string column".to_string())
                            })?
                            .to_string();
                        offset += str_len;
                        values.push(Value::String(s));
                    }
                    DataType::Bytes => {
                        if offset + 4 > bytes.len() {
                            return Err(Error::TupleCorruption(
                                "truncated bytes length".to_string(),
                            ));
                        }
                        let byte_len = u32::from_le_bytes([
                            bytes[offset],
                            bytes[offset + 1],
                            bytes[offset + 2],
                            bytes[offset + 3],
                        ]) as usize;
                        offset += 4;

                        if offset + byte_len > bytes.len() {
                            return Err(Error::TupleCorruption(format!(
                                "byte length {byte_len} exceeds tuple bounds (remaining: {})",
                                bytes.len() - offset
                            )));
                        }

                        let b = bytes[offset..offset + byte_len].to_vec();
                        offset += byte_len;
                        values.push(Value::Bytes(b));
                    }
                }
            }
        }

        if offset != bytes.len() {
            return Err(Error::TupleCorruption(format!(
                "trailing garbage: {} unconsumed bytes at end of tuple",
                bytes.len() - offset
            )));
        }

        let row = Row { values };
        Ok(row)
    }
}

/// A single logical tuple composed of typed scalar values.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// Ordered list of column values matching the table schema.
    pub values: Vec<Value>,
}

impl Row {
    /// Creates a new `Row` from a vector of `Value`s.
    pub fn new(values: Vec<Value>) -> Self {
        Self { values }
    }

    /// Returns a reference to the value at `index`, or `None` if out of bounds.
    pub fn get(&self, index: usize) -> Option<&Value> {
        self.values.get(index)
    }

    /// Returns the number of column values in this row.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Returns `true` if this row contains zero values.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
}
