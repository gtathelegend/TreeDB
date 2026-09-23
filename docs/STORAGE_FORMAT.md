# TreeDB On-Disk Storage Format

TreeDB stores all data in a single primary database file (`.treedb`) and an associated write-ahead log file (`.treedb.wal`).

* **Default Page Size:** 4096 bytes (4 KiB)
* **Byte Ordering:** Little-Endian for all multi-byte integers
* **Current Database Format Version:** `2`
* **Current WAL Format Version:** `1`

---

## Overall Database File Layout

```text
+-----------------------+ offset 0x0000 (0 bytes)
| Page 0: File Header   | (4096 bytes)
+-----------------------+ offset 0x1000 (4096 bytes)
| Page 1: Metadata Page | (4096 bytes)
+-----------------------+ offset 0x2000 (8192 bytes)
| Page 2: User Page     | (4096 bytes)
+-----------------------+
| ...                   |
+-----------------------+ offset (page_count * 4096)
```

---

## Page 0: Database File Header

Page 0 contains global configuration, integrity verification tokens, and allocation metadata.

| Byte Offset | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 8 bytes | `magic` | `[u8; 8]` | ASCII identifier `TREEDB01` |
| `0x08` | 4 bytes | `format_version` | `u32` (LE) | On-disk format version (`2`) |
| `0x0C` | 4 bytes | `page_size` | `u32` (LE) | Page size in bytes (`4096`) |
| `0x10` | 4 bytes | `page_count` | `u32` (LE) | Total number of allocated pages |
| `0x14` | 4 bytes | `root_page_id` | `u32` (LE) | Page ID of B+ Tree root (`0` if uninitialized) |
| `0x18` | 4 bytes | `metadata_page_id` | `u32` (LE) | Page ID of catalog/metadata page (`1`) |
| `0x1C` | 4 bytes | `freelist_head` | `u32` (LE) | Page ID of first free page in linked freelist (`0` if empty) |
| `0x20` | 4 bytes | `free_page_count` | `u32` (LE) | Total number of free pages currently in freelist |
| `0x24` | 8 bytes | `last_checkpoint_lsn`| `u64` (LE) | LSN of the most recent completed durable checkpoint (`0` if none) |
| `0x2C` | 4052 bytes | `reserved` | `[u8; 4052]`| Zero-padded reserved space for future extensions |

---

## Standard Page Header (Page 1+)

All pages other than Page 0 share a common 16-byte header:

```text
+---------------------------------------------------------------+
| Byte 0..3: Page ID (u32, LE)                                  |
+---------------------------------------------------------------+
| Byte 4: Page Type Tag (u8) | Byte 5: Flags (u8) | Byte 6..7: 0|
+---------------------------------------------------------------+
| Byte 8..15: Page LSN (u64, LE) - Most recent WAL log sequence |
+---------------------------------------------------------------+
| Byte 16..4095: Page Payload (4080 bytes)                      |
+---------------------------------------------------------------+
```

### Page Type Discriminants

| Tag (u8) | Name | Purpose |
| :--- | :--- | :--- |
| `0x01` | `Metadata` | Root catalog and table descriptor storage |
| `0x02` | `Internal` | B+ Tree interior routing node |
| `0x03` | `Leaf` | B+ Tree leaf node storing keys and record pointers |
| `0x04` | `Record` | Slotted data page holding tuple data |
| `0x05` | `Free` | Unallocated / recycled page in persistent freelist |
| `0x06` | `Overflow` | Multi-page overflow chunk for values > 4070 bytes |
| `0x07` | `Catalog` | Dedicated catalog page for table schema metadata |

---

## Catalog Page Payload Layout (`PageType::Catalog` / `PageType::Metadata`)

The 4080-byte payload of a Catalog page is structured as:

### Catalog Header (32 bytes)
| Byte Offset | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 4 bytes | `magic` | `[u8; 4]` | ASCII identifier `CATL` |
| `0x04` | 4 bytes | `format_version` | `u32` (LE) | Catalog layout version (`1`) |
| `0x08` | 4 bytes | `next_table_id` | `u32` (LE) | Monotonic counter for allocating new `TableId`s |
| `0x0C` | 4 bytes | `table_count` | `u32` (LE) | Number of active table metadata entries on this page |
| `0x10` | 4 bytes | `next_catalog_page_id`| `u32` (LE) | PageId of next chained catalog page (`0` if end of chain) |
| `0x14` | 12 bytes | `reserved` | `[u8; 12]` | Zero-filled reserved space |

### Table Metadata Entry Layout (Variable length, 26 bytes + name_len + column definitions)
| Byte Offset | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `+0x00` | 4 bytes | `table_id` | `u32` (LE) | Unique, stable logical table identifier |
| `+0x04` | 4 bytes | `schema_version` | `u32` (LE) | Table schema definition version (`1`) |
| `+0x08` | 4 bytes | `root_page_id` | `u32` (LE) | Root PageId of table's B+ Tree index |
| `+0x0C` | 4 bytes | `record_page_id` | `u32` (LE) | Initial/active PageId of table's slotted RecordStore |
| `+0x10` | 4 bytes | `flags` | `u32` (LE) | Configuration flags for future schema options |
| `+0x14` | 2 bytes | `primary_key_idx` | `u16` (LE) | Index of primary key column in `columns` array |
| `+0x16` | 2 bytes | `column_count` | `u16` (LE) | Number of columns in the schema (`N`) |
| `+0x18` | 2 bytes | `name_len` | `u16` (LE) | Length in bytes of table name (1..64) |
| `+0x1A` | `name_len` bytes | `name` | `[u8; name_len]` | UTF-8 encoded table name string |
| Following | Variable | `columns` | `[ColumnDef; N]` | Sequence of serialized column definitions |

#### Column Definition Layout (`4 + col_name_len` bytes per column)
| Byte Offset | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `+0x00` | 1 byte | `data_type` | `u8` | Data type tag (`1`=Boolean, `2`=Int64, `3`=UInt64, `4`=Float64, `5`=String, `6`=Bytes) |
| `+0x01` | 1 byte | `nullable` | `u8` | `0` = Not Null, `1` = Nullable |
| `+0x02` | 2 bytes | `col_name_len` | `u16` (LE) | Length in bytes of column name (1..64) |
| `+0x04` | `col_name_len` bytes | `col_name` | `[u8; col_name_len]` | UTF-8 encoded column name string |

---

## Binary Tuple Serialization Format (Version 1)

Tuples stored in the RecordStore and indexed by the B+ Tree are serialized deterministically as:

```text
+---------------------------------------------------------------+
| Byte 0: format_version (u8 = 1)                               |
+---------------------------------------------------------------+
| Byte 1..2: column_count (u16, LE)                             |
+---------------------------------------------------------------+
| Byte 3..3+ceil(N/8): Null Bitmap (1 = NULL, 0 = Present)      |
+---------------------------------------------------------------+
| Following: Packed Column Values in Schema Order               |
|  - Boolean: 1 byte (0 or 1)                                   |
|  - Int64:   8 bytes (i64, Little-Endian)                      |
|  - UInt64:  8 bytes (u64, Little-Endian)                      |
|  - Float64: 8 bytes (IEEE 754 f64 bits, Little-Endian)        |
|  - String:  4 bytes length (u32, LE) + UTF-8 payload          |
|  - Bytes:   4 bytes length (u32, LE) + raw byte payload       |
+---------------------------------------------------------------+
```

* **Canonical Primary Key Indexing:** For signed integer primary keys (`DataType::Int64`), the engine applies a sign-flip transformation `canonical_key = (i as u64) ^ (1 << 63)`. This maps `[i64::MIN, i64::MAX]` monotonically onto `[0, u64::MAX]`, enabling negative integers to sort naturally before positive integers in the unsigned B+ Tree.

---

## Free Page Payload Layout (`PageType::Free`)

The 4080-byte payload of a Free page is structured as:

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 4 bytes | `next_free` | `u32` (LE) | PageId of next free page in singly-linked freelist (`0` if end of list) |
| `0x04` | 4076 bytes | `padding` | `[u8; 4076]` | Zero-filled padding |

---

## Overflow Page Payload Layout (`PageType::Overflow`)

The 4080-byte payload of an Overflow page is structured as:

```text
+---------------------------------------------------------------+
| Byte 0..3: next_page_id (u32, LE) - Next page in chain (0=end)|
+---------------------------------------------------------------+
| Byte 4..5: bytes_used (u16, LE) | Byte 6..7: reserved (u16=0) |
+---------------------------------------------------------------+
| Byte 8..8+bytes_used: Usable Payload Data (up to 4072 bytes)  |
+---------------------------------------------------------------+
| Remainder: Zero-filled padding                                |
+---------------------------------------------------------------+
```

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 4 bytes | `next_page_id` | `u32` (LE) | PageId of next overflow page in singly-linked chain (`0` if final page) |
| `0x04` | 2 bytes | `bytes_used` | `u16` (LE) | Number of valid payload bytes on this page (0..=4072) |
| `0x06` | 2 bytes | `reserved` | `u16` (LE) | Zero-filled reserved field |
| `0x08` | `bytes_used` bytes | `data` | `[u8]` | Chunk payload data (max `OVERFLOW_PAYLOAD_SIZE = 4072 bytes`) |
| `0x08 + bytes_used` | Remainder | `padding` | `[u8]` | Zero-filled padding |

* **Usable Capacity:** `OVERFLOW_PAYLOAD_SIZE = 4080 (page payload) - 8 (overflow header) = 4072 bytes`.

---

## B+ Tree Leaf Node Payload Layout (`PageType::Leaf`)

The 4080-byte payload of a Leaf page is structured as:

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 2 bytes | `key_count` | `u16` (LE) | Number of active key-record entries (0..254) |
| `0x02` | 4 bytes | `next_leaf` | `u32` (LE) | PageId of next sibling leaf (`0` if none) |
| `0x06` | 16 bytes × `N` | `entries` | `[(Key, RecordId)]` | Array of `[Key (8B) \| RecordId (8B)]` |
| `0x06 + (16 × N)` | Remainder | `padding` | `[u8]` | Unused space (zero-filled) |

* **Capacity:** Max 254 entries = `6 + (254 × 16) = 4070 bytes` (10 bytes padding).
* **RecordId:** `(page_id: u32, slot_id: u32)` = 8 bytes.

---

## B+ Tree Internal Node Payload Layout (`PageType::Internal`)

The 4080-byte payload of an Internal page is structured as:

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 2 bytes | `key_count` | `u16` (LE) | Number of separator keys `N` (0..339) |
| `0x02` | 4 bytes | `first_child` | `u32` (LE) | PageId of leftmost child pointer `C0` |
| `0x06` | 12 bytes × `N` | `entries` | `[(Key, PageId)]` | Array of `[SeparatorKey (8B) \| ChildPageId (4B)]` |
| `0x06 + (12 × N)` | Remainder | `padding` | `[u8]` | Unused space (zero-filled) |

* **Capacity:** Max 339 separator keys (340 children) = `6 + (339 × 12) = 4074 bytes` (6 bytes padding).

---

## Slotted Record Page Payload Layout (`PageType::Record`)

The 4080-byte payload of a Record page is structured using a slotted-page layout:

```text
+---------------------------------------------------------------+
| Byte 0..5: Record Page Header (6 bytes)                       |
|   slot_count: u16 (2B) | free_start: u16 (2B) | free_end: u16 |
+---------------------------------------------------------------+
| Byte 6..free_start: Slot Directory (grows UPWARD)             |
|   Slot 0: [offset: u16 (2B) | length: u16 (2B, bit 15 flag)]  |
|   Slot 1: [offset: u16 (2B) | length: u16 (2B)]               |
|   ...                                                         |
+---------------------------------------------------------------+
| Byte free_start..free_end: Free Space Region (contiguous gap) |
+---------------------------------------------------------------+
| Byte free_end..4080: Payloads & Descriptors (grows DOWNWARD)  |
|   Record N payload / [head_page_id: u32 | total_len: u32]     |
|   ...                                                         |
|   Record 0 payload                                            |
+---------------------------------------------------------------+
```

### Record Page Header Fields

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 2 bytes | `slot_count` | `u16` (LE) | Total number of slots in directory |
| `0x02` | 2 bytes | `free_space_start` | `u16` (LE) | Offset of free space beginning (`6 + slot_count * 4`) |
| `0x04` | 2 bytes | `free_space_end` | `u16` (LE) | Offset of free space end (top of record payloads) |

### Slot Directory Entry Format (4 bytes each)

| Field Name | Length | Type | Description |
| :--- | :--- | :--- | :--- |
| `offset` | 2 bytes | `u16` (LE) | Byte offset within page payload (`0` if deleted) |
| `length` | 2 bytes | `u16` (LE) | Byte length of record payload or overflow descriptor with `OVERFLOW_FLAG` (`0x8000`) |

* **Deleted Slot Sentinel:** `offset == 0 && length == 0` marks an unused/deleted slot.
* **Inline Record Encoding:** `length & 0x8000 == 0`, where `length <= 4070` bytes. The payload at `offset..offset+length` is the inline record value.
* **Overflow Record Encoding:** `length & 0x8000 != 0` (`0x8008`). The on-page payload is an 8-byte descriptor `[head_page_id: u32 (4B) | total_length: u32 (4B)]`.
* **Maximum Single-Page Inline Size:** `MAX_RECORD_SIZE = 4080 - 6 - 4 = 4070 bytes`.
* **RecordId:** `(page_id: u32, slot_id: u32)`. Logical `RecordId`s remain stable across page compactions for both inline and overflow records.

### Maximum Record Size Evolution

The maximum payload size of a single record is derived directly from the page layout:

* **Phase 6 (Format Version 1):**
  `4096 (page size) - 8 (page header) - 6 (record page header) - 4 (slot directory entry) = 4078 bytes`
* **Phase 7 / 7.1 / Phase 8 (Format Version 2):**
  `4096 (page size) - 16 (page header with PageLSN) - 6 (record page header) - 4 (slot directory entry) = 4070 bytes` (single-page limit)
* **Phase 9 (Format Version 2 with Multi-Page Overflow):**
  Values $\le 4070$ bytes are stored inline on slotted record pages. Values $> 4070$ bytes span an arbitrary number of `PageType::Overflow` pages (4072 bytes per page), referenced by an 8-byte on-page descriptor. Single-page limitation is eliminated.

---

## Write-Ahead Log File Format (`.wal`)

The WAL is stored in a separate file with extension `.wal` alongside the primary database file.

### WAL Header (32 bytes)

| Byte Offset | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 8 bytes | `magic` | `[u8; 8]` | ASCII identifier `TREEWAL1` |
| `0x08` | 4 bytes | `version` | `u32` (LE) | WAL format version (`1`) |
| `0x0C` | 8 bytes | `flushed_lsn` | `u64` (LE) | Highest LSN flushed to durable storage |
| `0x14` | 12 bytes | `reserved` | `[u8; 12]` | Zero-padded reserved bytes |

### Self-Delimiting Log Record Layout

```text
+--------------------------------------------------------------------------------+
| Total Payload Len (u32, 4B)                                                    |
+--------------------------------------------------------------------------------+
| LSN (u64, 8B) | Prev LSN (u64, 8B) | Txn ID (u64, 8B) | Record Type Tag (u8, 1B)|
+--------------------------------------------------------------------------------+
| Record Body Bytes (variable length)                                            |
+--------------------------------------------------------------------------------+
| IEEE 802.3 CRC32 Checksum (u32, 4B)                                            |
+--------------------------------------------------------------------------------+
```

### Log Record Types

| Tag (u8) | Type | Body Layout |
| :--- | :--- | :--- |
| `0x01` | `Begin` | Empty (0 bytes) |
| `0x02` | `Commit` | Empty (0 bytes) |
| `0x03` | `Abort` | Empty (0 bytes) |
| `0x04` | `PageDelta` | `page_id (4B)` + `offset (2B)` + `before_len (2B)` + `before_bytes` + `after_len (2B)` + `after_bytes` |
| `0x05` | `Clr` | `page_id (4B)` + `offset (2B)` + `undo_next_lsn (8B)` + `redo_len (2B)` + `redo_bytes` |
| `0x06` | `Checkpoint`| `num_txns (4B)` + array of `[txn_id (8B) \| last_lsn (8B)]` |
