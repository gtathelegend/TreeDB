# TreeDB On-Disk Storage Format

TreeDB stores all data in a single file with fixed-size pages.

* **Default Page Size:** 4096 bytes (4 KiB)
* **Byte Ordering:** Little-Endian for all multi-byte fields

---

## Overall File Layout

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
| `0x08` | 4 bytes | `format_version` | `u32` (LE) | On-disk format version (currently `1`) |
| `0x0C` | 4 bytes | `page_size` | `u32` (LE) | Page size in bytes (`4096`) |
| `0x10` | 4 bytes | `page_count` | `u32` (LE) | Total number of allocated pages |
| `0x14` | 4 bytes | `root_page_id` | `u32` (LE) | Page ID of B+ Tree root (`0` if uninitialized) |
| `0x18` | 4 bytes | `metadata_page_id` | `u32` (LE) | Page ID of catalog/metadata page (`1`) |
| `0x1C` | 4068 bytes | `reserved` | `[u8; 4068]`| Zero-padded reserved space for future extensions |

---

## Standard Page Header (Page 1+)

All pages other than Page 0 share a common 8-byte header:

```text
+---------------------------------------------------------------+
| Byte 0..3: Page ID (u32, LE)                                  |
+---------------------------------------------------------------+
| Byte 4: Page Type Tag (u8) | Byte 5: Flags (u8) | Byte 6..7: 0|
+---------------------------------------------------------------+
| Byte 8..4095: Page Payload (4088 bytes)                       |
+---------------------------------------------------------------+
```

### Page Type Discriminants

| Tag (u8) | Name | Purpose |
| :--- | :--- | :--- |
| `0x01` | `Metadata` | Catalog and table descriptor storage |
| `0x02` | `Internal` | B+ Tree interior routing node |
| `0x03` | `Leaf` | B+ Tree leaf node storing keys and record pointers |
| `0x04` | `Record` | Slotted data page holding tuple data |
| `0x05` | `Free` | Unallocated / recycled page |

---

## B+ Tree Leaf Node Payload Layout (`PageType::Leaf`)

The 4088-byte payload of a Leaf page is structured as:

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 2 bytes | `key_count` | `u16` (LE) | Number of active key-record entries (0..255) |
| `0x02` | 4 bytes | `next_leaf` | `u32` (LE) | PageId of next sibling leaf (`0` if none) |
| `0x06` | 16 bytes × `N` | `entries` | `[(Key, RecordId)]` | Array of `[Key (8B) \| RecordId (8B)]` |
| `0x06 + (16 × N)` | Remainder | `padding` | `[u8]` | Unused space (zero-filled) |

* **Capacity:** Max 255 entries = `6 + (255 × 16) = 4086 bytes` (2 bytes padding).
* **RecordId:** `(page_id: u32, slot_id: u32)` = 8 bytes.

---

## B+ Tree Internal Node Payload Layout (`PageType::Internal`)

The 4088-byte payload of an Internal page is structured as:

| Byte Offset (in payload) | Length | Field Name | Type | Description |
| :--- | :--- | :--- | :--- | :--- |
| `0x00` | 2 bytes | `key_count` | `u16` (LE) | Number of separator keys `N` (0..340) |
| `0x02` | 4 bytes | `first_child` | `u32` (LE) | PageId of leftmost child pointer `C0` |
| `0x06` | 12 bytes × `N` | `entries` | `[(Key, PageId)]` | Array of `[SeparatorKey (8B) \| ChildPageId (4B)]` |
| `0x06 + (12 × N)` | Remainder | `padding` | `[u8]` | Unused space (zero-filled) |

* **Capacity:** Max 340 separator keys (341 children) = `6 + (340 × 12) = 4086 bytes` (2 bytes padding).

---

## Slotted Record Page Payload Layout (`PageType::Record`)

The 4088-byte payload of a Record page is structured using a slotted-page layout:

```text
+---------------------------------------------------------------+
| Byte 0..5: Record Page Header (6 bytes)                       |
|   slot_count: u16 (2B) | free_start: u16 (2B) | free_end: u16 |
+---------------------------------------------------------------+
| Byte 6..free_start: Slot Directory (grows UPWARD)             |
|   Slot 0: [offset: u16 (2B) | length: u16 (2B)]               |
|   Slot 1: [offset: u16 (2B) | length: u16 (2B)]               |
|   ...                                                         |
+---------------------------------------------------------------+
| Byte free_start..free_end: Free Space Region (contiguous gap) |
+---------------------------------------------------------------+
| Byte free_end..4088: Record Payloads (grows DOWNWARD)         |
|   Record N payload                                            |
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
| `length` | 2 bytes | `u16` (LE) | Byte length of record payload |

* **Deleted Slot Sentinel:** `offset == 0` marks an unused/deleted slot.
* **Maximum Record Size:** `MAX_RECORD_SIZE = 4088 - 6 - 4 = 4078 bytes`.
* **RecordId:** `(page_id: u32, slot_id: u32)`. Logical `RecordId`s remain stable across page compactions.
