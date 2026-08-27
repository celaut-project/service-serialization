# SeSe (service serialization protocol)
A deterministic transport protocol designed for the Celaut ecosystem. It standardizes communication interfaces for node-to-node and service-to-node interactions, ensuring complete decoupling between decentralized infrastructure and services.

# Protocol Specification

The Simple Service Serialization protocol is a binary serialization format optimized for **zero-copy access** and **content-addressed storage**. It maps logical fields to specific byte offsets to ensure $O(1)$ access without requiring a full deserialization pass.

## 1. Primitive Type Encoding

All integers are encoded in **Little-Endian** format.

| Type | Size (Bytes) | Description |
| --- | --- | --- |
| `bool` | 1 | `0x00` = false, `0x01` = true |
| `u64` | 8 | Unsigned 64-bit integer |
| `bytes` | $8 + N$ | `u64` length followed by $N$ bytes of data |
| `string` | $8 + N$ | `u64` length followed by UTF-8 bytes |
| `list<T>` | $8 + (N \times M)$ | `u64` count followed by $N$ elements |

## 2. File Architecture

The file is structured into three distinct regions:

1. **Body (Data Region):** Contains the actual data segments. Fields are written sequentially.
2. **Footer (Offset Table):** An array of `u64` pointers indicating the start position of each field defined in the schema.
3. **Trailer (Control Region):** The last 16 bytes of the file.
* `u64`: Offset to the start of the Footer.
* `u64`: Magic Number (`0x46465F564D5F4D50` - "FF_VM_MP").



## 3. The "Flat-Footer" Layout

The memory layout for a message with $N$ fields:

```text
[ FIELD_1_DATA ]
[ FIELD_2_DATA ]
...
[ FIELD_N_DATA ]
[ FOOTER_START_PTR (Offset to Table) ]  <-- (Pointed to by Trailer)
[ OFFSET_TO_FIELD_1 ]
[ OFFSET_TO_FIELD_2 ]
...
[ MAGIC_NUMBER (8 bytes) ]              <-- (Fixed position: EOF-8)
[ FOOTER_PTR (8 bytes) ]                <-- (Fixed position: EOF-16)

```

---

## 4. Implementation Example (Rust)

This snippet demonstrates how to map an existing file and perform zero-copy access to the "filesystem" field.

```rust
use memmap2::Mmap;
use std::fs::File;

struct ServiceReader {
    mmap: Mmap,
}

impl ServiceReader {
    pub fn new(path: &str) -> Self {
        let file = File::open(path).unwrap();
        let mmap = unsafe { Mmap::map(&file).unwrap() };
        Self { mmap }
    }

    /// Accesses the N-th field using the Footer offsets
    pub fn get_field_offset(&self, field_index: usize) -> u64 {
        let file_len = self.mmap.len();
        
        // 1. Read Footer Pointer (Last 16 bytes)
        let footer_ptr_bytes = &self.mmap[file_len - 16..file_len - 8];
        let footer_ptr = u64::from_le_bytes(footer_ptr_bytes.try_into().unwrap());
        
        // 2. Read specific offset from Footer table
        let offset_start = footer_ptr as usize + (field_index * 8);
        let field_offset = u64::from_le_bytes(self.mmap[offset_start..offset_start + 8].try_into().unwrap());
        
        field_offset
    }

    /// Access data without copying (Zero-Copy)
    pub fn get_data_slice(&self, field_index: usize) -> &[u8] {
        let start = self.get_field_offset(field_index) as usize;
        
        // Data is stored as [Length: u64][Raw Data]
        let len_bytes = &self.mmap[start..start + 8];
        let len = u64::from_le_bytes(len_bytes.try_into().unwrap()) as usize;
        
        &self.mmap[start + 8..start + 8 + len]
    }
}

```

---

## 5. Formal Identity and Deduplication

* **Hash Identity:** The unique service ID is calculated as: `BLAKE3(FileContent)`. Since the structure is fixed and offset-based, identical files will always produce identical hashes.
* **Streaming/Deduplication:** Because the protocol uses deterministic offsets, the data body can be treated as a collection of blocks. A transport layer can hash these blocks independently without needing to "parse" the file, enabling efficient deduplication across different VMs.



======



### Executive Summary

To achieve **extreme simplicity, O(1) memory access, and cryptographic determinism** for your Virtual Machine (VM) specifications, you are moving away from serialized formats (Protobuf/FlatBuffers) toward a **Flat-Footer (FF)** architecture.

#### 1. Why skip Protobuf and FlatBuffers?

| Feature | Protobuf | FlatBuffers | **Flat-Footer (FF)** |
| --- | --- | --- | --- |
| **Parsing** | High (CPU decoding required) | Low (V-Table navigation) | **None (Pointer math only)** |
| **Determinism** | No (Field ordering/Varints) | No (Compiler-dependent) | **Absolute (Fixed Layout)** |
| **Memory** | $O(N)$ (Buffer copies) | $O(1)$ (Mmap + Padding) | **$O(1)$ (Mmap Direct)** |
| **Complexity** | High (Needs runtime library) | Medium (Needs code-gen) | **Minimal (Raw structures)** |

* **Protobuf:** Designed for network agility, not VM performance. Its use of *Varints* and sequential tagging requires a full CPU pass to find the end of a field, making it impossible to perform true zero-copy access on large datasets.
* **FlatBuffers:** While it supports zero-copy, it is a "black box." It relies on compiler-generated V-Tables, which makes the binary format non-deterministic (different versions of the compiler can produce different binaries). Furthermore, it adds the complexity of v-table navigation and dynamic padding, which you are consciously choosing to trade for absolute control.

#### 2. The Core Philosophy of Flat-Footer

Instead of "parsing" a stream, you treat the binary file as an **addressable memory map**.

* **Identity via Hash:** The service ID is `BLAKE3(FileContent)`. Because the layout is rigid and position-based, the hash is immutable and globally reproducible.
* **Zero-Copy Access:** Using `mmap`, you map the file into process space. Accessing any field is a simple `base_ptr + offset` calculation ($O(1)$).
* **Content-Addressable Storage (CAS):** By using fixed-size blocks for your deduplication logic, you treat the VM body as a collection of chunks. You only transfer/store the blocks the peer is missing.

#### 3. Formal Protocol Specification

* **Data Encoding:** Little-Endian for all numeric primitives.
* **Nesting via Pointers:** Objects are nested using **absolute offsets**. A parent object stores a `u64` address pointing to the start of a child block. This maintains logical hierarchy without physical interdependency.
* **Layout:**
* **Body:** Sequential data fields.
* **Footer:** A `u64` array at the end of the file containing the start offsets of all fields.
* **Trailer (Last 16 bytes):** Contains the pointer to the `Footer` and a `Magic Number`.



#### 4. Practical Implementation (Rust)

Because your format is rigid, you can define your accessors as direct memory slices:

```rust
// Accessing a nested field: No parsing, no allocation, no overhead.
pub fn get_field(mmap: &Mmap, offset: u64, length: u64) -> &[u8] {
    let start = offset as usize;
    &mmap[start..start + length as usize]
}

```



=====



Transmission over QUIC uses the footer as a header.  Negotiates blocks before transmission to consider what parts of the buffer are not needed to be transmited. In case there are no blocks, can transmit using compression ZStandard.

