# SeSe v0: byte-level specification (as implemented)

This is the precise format that the crates in this repository read and write. It follows
the concept in `README.md`. Where the README is open or contradicts itself, this file picks
the simplest consistent reading and marks the choice with **[Dn]**. Each choice is also a
question in the pull request ("Questions for Josemi"), with the default proposed here.

Words: *MUST* = the reader refuses input that breaks it. *Canonical* = what the writer
produces; one logical value has exactly one canonical encoding.

## 1. Conventions

- All integers are unsigned 64-bit little-endian (`u64`), except the 1-byte `bool`.
- **All offsets are absolute: they count from byte 0 of the file**, also inside nested
  messages. **[D5]**
- `ABSENT = 0xFFFF_FFFF_FFFF_FFFF`: the offset of a field that is not present. **[D4]**
- `MAGIC = 0x46465F564D5F4D50` ("FF_VM_MP"), written as a little-endian u64 like every
  integer, so the 8 bytes on disk are `50 4D 5F 4D 56 5F 46 46` ("PM_MV_FF"). **[D1]**

## 2. Message

A message is a region of bytes `[start, end)`. The root message is the whole file
(`start = 0`, `end = file length`).

```text
start                                                                        end
  | body: field values, in index order, with alignment padding | footer | trailer |
                                                               ^
                                                         footer_ptr
  footer  = N entries, u64 each: entry i = offset of the value of field i, or ABSENT
  trailer = footer_ptr: u64 (at end-16), MAGIC: u64 (at end-8)
```

- The trailer is the last 16 bytes: the footer pointer at `end-16`, then the magic at
  `end-8`. **[D2]** The footer does not start with a pointer to itself. **[D3]**
- MUST: `end - start >= 16`; the magic matches; `start <= footer_ptr <= end - 16`;
  `(end - 16 - footer_ptr) % 8 == 0`. Then `N = (end - 16 - footer_ptr) / 8`.
- The body is `[start, footer_ptr)`. MUST: a field offset that is not ABSENT is in the body,
  and the whole value (length prefix and data) is in the body.
- A field index `>= N` reads as absent (a newer reader, an older file). Footer entries
  past the schema of the reader are ignored (an older reader, a newer file).

## 3. Values

The file does not describe its types: the schema of the reader gives the type of each
field index.

| Type | Encoding at the offset `o` | Reader checks (MUST) |
|---|---|---|
| `bool` | 1 byte: `00` or `01` | other bytes refused |
| `u64` | 8 bytes | in the body |
| `bytes` | `u64 L`, then `L` bytes | `o + 8 + L` in the body, no overflow |
| `string` | `u64 L`, then `L` bytes of UTF-8 | as bytes, and valid UTF-8 |
| `message` | `u64 L`, then a message region `[o+8, o+8+L)` (section 2) | region in the body; its own trailer |
| `list<bool>` | `u64 C`, then `C` bytes | `C` bytes in the body, each `00`/`01` |
| `list<u64>` | `u64 C`, then `C` × 8 bytes | `C × 8` in the body, no overflow |
| `list<bytes / string / message>` | `u64 C`, then `C` × u64 offsets, each to an element encoded as above | the table in the body before any allocation; each element offset and element in the body |

- A nested message is the "pointer to a child block" of the README: the parent's footer
  entry points to `[L][child region]`, and the child region has its own footer and trailer,
  with absolute offsets. **[D6]**
- A list of variable-size elements has an offset table, so element `k` is O(1). **[D7]**

## 4. Canonical encoding (the writer)

The writer appends to one buffer:

1. For each field index in ascending order that is present: padding (only for an aligned
   `bytes`, below), then the value. The footer has one entry for each index of the schema
   (absent → ABSENT). The footer follows the last value; the trailer follows the footer.
2. A nested message region starts right after its length; its fields follow the same rules.
3. A variable-size list: the count, the offset table, then the elements in order.
4. **Alignment [D9]:** a `bytes` value (a field or a list element) with `L >= ALIGN_MIN`
   (65 536) gets zero bytes before its length so that its *data* starts at a multiple of
   `BLOCK_SIZE` (16 384), and zero bytes after its data up to the next multiple of
   `BLOCK_SIZE`. There is no other padding. Strings are not aligned.

So the bytes are a function of the logical value. `value::is_canonical` decodes a file and
encodes it again to check that a file is canonical.

Alignment is what makes fixed-size blocks useful across versions: a large file of a
service fills whole blocks of its own, so its blocks are the same wherever it is in the
file, and an edit elsewhere does not change them. After an aligned value the layout is at
a block boundary again, so a shift in the small values before it stops there.

## 5. Identity and blocks

- **Id** = BLAKE3-256 of all the bytes of the file.
- **Block** `k` = bytes `[k·B, min((k+1)·B, len))` with `B = BLOCK_SIZE = 16 384`; the last
  block can be shorter. Its hash is BLAKE3-256 of its bytes. No parse is needed to find
  blocks.

## 6. Proto messages and the Celaut `Service` **[D8, D10–D13]**

`sese-celaut` maps any message of `celaut.proto` (nodo `dev`, copied in
`crates/sese-celaut/protos/`) with its descriptor:

- **Field number `n` is footer index `n - 1`.** `N` = the largest field number of the
  message. Unused numbers are ABSENT. **[D10]**
- `bool` → bool. `int32/int64/sint*/sfixed*/enum` → u64 holding the value as i64 two's
  complement. `uint32/uint64/fixed*` → u64. `float` → u64 holding the 32 IEEE bits,
  `double` → u64 holding the 64 bits. `string` → string. `bytes` → bytes. message → message.
  `repeated` → the list of the element type. `map` → not supported (refused; Service has
  none). **[D8]**
- Presence **[D11]**: a field without presence (proto3 plain scalar, string, bytes) is
  always written, also at its default value. A field with presence (message, `optional`,
  oneof member) is written only when set. A repeated field is always written.
- On the way back, a value out of the range of its proto type (an int32 that is not a
  sign-extended i32, a uint32 above 2³²−1, a float with high bits), two members of one
  oneof, or a missing message is refused.
- **`Service.Container.filesystem`** (a `bytes` field that holds a serialized `Filesystem`)
  is written as the nested `Filesystem` message, so every file of the service is a `bytes`
  value of its own (aligned when large) and a reader walks the tree with no proto parse
  (`walk_filesystem`). The proto bytes MUST be the canonical proto encoding of the
  `Filesystem` (fields in number order, no unknown fields); otherwise the mapping is refused,
  because the way back would not give the same bytes. **[D12]**
- Unknown proto fields are refused (they cannot be kept).
- The SeSe file of a service is the `Service` alone. The `Metadata` part of a
  `.celaut.bee` is not in it. **[D13]**
- Way back: `sese_to_service(service_to_sese(p)) == p` for a canonical `p`, so the Celaut
  service id (SHA3-256 of the proto bytes) stays the same. Checked on the real echo-chat
  package `b4e0438b…`.

## 7. Transport over QUIC (`sese-quic`) **[D14–D17]**

TLS 1.3, ALPN `sese/1`. One transfer = one bidirectional stream, opened by the sender.
All integers little-endian.

**HEADER** (sender → receiver):

| Bytes | Field |
|---|---|
| 4 | `"SESQ"` |
| 1 | version = `1` |
| 32 | id (BLAKE3 of the file) |
| 8 | file length `F` |
| 4 | block size `B` (a power of two, 4 KiB … 4 MiB) |
| 4 | tail length `T` (16 ≤ T ≤ 1 MiB, T ≤ F) |
| T | the tail: bytes `[F−T, F)` of the file = the root footer and the trailer |
| 8 | block count `K` = ⌈F / B⌉ |
| 32·K | the BLAKE3 hash of each block |

"QUIC uses the footer as a header": the tail is the root footer, so the receiver knows the
root field offsets before any data. MUST: the tail ends with the magic, and its footer
pointer equals `F − T` with `(T − 16) % 8 == 0`. The receiver refuses `F` above its limit.

**WANT** (receiver → sender): `00`, then ⌈K/8⌉ bytes of bitmap: bit `k % 8` of byte `k / 8`
is 1 when the receiver wants block `k` (it does not hold a block with that hash). Or `01`
and a UTF-8 reason, then the end of the direction (refused).

**DATA** (sender → receiver), one mode byte:

- `01` (only when every block is wanted, "zstd when no blocks" **[D14]**): one zstd stream
  of the whole file, until the end of the sender's direction. The receiver cuts the output
  into blocks and checks each block hash as it completes. The decoder window is limited to
  2²⁴ bytes.
- `00`: for each wanted block in ascending order: codec `u8` (`00` raw, `01` zstd),
  length `u32`, payload. The sender uses zstd only when it is smaller. The receiver refuses a
  payload longer than 1.5·B + 1024, a zstd payload that does not give exactly the block
  length, and anything after the last block.

Then the sender ends its direction.

**RESULT** (receiver → sender): `00` when the file is written and its BLAKE3 equals the id;
or `01` and a UTF-8 reason. The receiver checks every received block against its hash
**before** it writes it, takes held blocks from its store, and checks the id of the whole
file at the end.

**Resume [D16]:** the receiver writes to `out.partial`, in block order, only verified
blocks. When the connection is cut (a QUIC error), `out.partial` stays; the next transfer to
the same `out` indexes it as held blocks, so only the rest is sent. After a hash or id error
`out.partial` is removed.

Limits: the receiver allocates at most the header (≤ 1 MiB tail + 32 bytes per block of a
file up to its limit, 16 GiB by default) and one block at a time.

## 8. Reader safety

- Every read is bounds-checked with checked arithmetic against the body of its message;
  every length and count is checked against the bytes that remain before memory is reserved.
- The reader never panics; `clippy::unwrap_used`, `expect_used`, `panic` and
  `indexing_slicing` are denied in the `sese` crate outside tests.
- A nested region is strictly inside its parent's body, so nesting is finite. Recursive
  helpers stop at `MAX_DEPTH = 64` **[D15]**; the lazy reader reads one level at a time.
- Proptest fuzzes encode/decode round trips, one-byte mutations, truncations, extensions and
  random bytes with random schemas (`crates/sese/tests/roundtrip.rs`).

## 9. Golden vectors

`crates/sese/tests/vectors/*.sese` with their ids in `ids.txt`. Example, `scalars.sese`
(fields: bool true, u64 42, absent, string "hi", bytes 01 02 03), 86 bytes:

```text
00: 01                                  field 0 bool
01: 2a 00 00 00 00 00 00 00             field 1 u64 = 42
09: 02 00 00 00 00 00 00 00 68 69       field 3 string "hi"
13: 03 00 00 00 00 00 00 00 01 02 03    field 4 bytes
1e: 00.. 01.. ff ff ff ff ff ff ff ff 09.. 13..   footer: offsets 0, 1, ABSENT, 9, 19
46: 1e 00 00 00 00 00 00 00             footer pointer = 30
4e: 50 4d 5f 4d 56 5f 46 46             MAGIC
```

## 10. Decisions on the README (index)

| # | README says | This spec |
|---|---|---|
| D1 | magic `0x46465F564D5F4D50` "FF_VM_MP", all integers LE | the u64 written LE: bytes "PM_MV_FF" |
| D2 | diagram: MAGIC then FOOTER_PTR, but labels MAGIC at EOF−8 and FOOTER_PTR at EOF−16; text and code: pointer then magic | pointer at EOF−16, magic at EOF−8 (text + code) |
| D3 | diagram shows `FOOTER_START_PTR` as the first footer row | no self-pointer: the trailer's pointer is the only one (as the code reads the table) |
| D4 | no way to mark an absent field | offset `0xFFFF…FFFF` |
| D5 | "objects are nested using absolute offsets" | absolute from byte 0 of the file, also inside nested messages |
| D6 | "a parent stores a u64 address pointing to the start of a child block" | the parent's footer entry → `[u64 length][child region with its own footer and trailer]` |
| D7 | `list<T>` = count + N×M | fixed-size T inline; variable-size T as an offset table |
| D8 | only `bool` and `u64` | every proto integer, enum, float and double in a u64 |
| D9 | "fixed-size blocks", no size given | 16 KiB blocks; large bytes values aligned (≥ 64 KiB) |
| D10 | no schema for a Celaut Service | footer index = proto field number − 1 |
| D11 | — | proto3 presence rules above |
| D12 | — | `Container.filesystem` as a nested message; canonical proto required |
| D13 | id = BLAKE3(FileContent) | the file is the Service only; Metadata outside |
| D14 | "zstd when no blocks" | whole-file zstd when the receiver holds none; else per-block zstd when smaller |
| D15 | — | recursive helpers limited to depth 64 |
| D16 | — | resume from the `.partial` file |
| D17 | "footer as a header", "negotiates blocks before transmission" | header = tail (root footer + trailer) + all block hashes; answer = bitmap |
