//! Round trips (proptest) and the reader API on encoded values.
mod common;

use proptest::prelude::*;
use sese::value::{decode, encode, is_canonical, Schema, Type, Value};
use sese::{Message, WriterOptions};

fn arb_type(depth: u32) -> BoxedStrategy<Type> {
    let leaf = prop_oneof![
        Just(Type::Bool),
        Just(Type::U64),
        Just(Type::Bytes),
        Just(Type::Str),
        Just(Type::ListBool),
        Just(Type::ListU64),
        Just(Type::ListBytes),
        Just(Type::ListStr),
    ];
    if depth == 0 {
        return leaf.boxed();
    }
    prop_oneof![
        4 => leaf,
        1 => arb_schema(depth - 1).prop_map(Type::Message),
        1 => arb_schema(depth - 1).prop_map(Type::ListMessage),
    ]
    .boxed()
}

fn arb_schema(depth: u32) -> BoxedStrategy<Schema> {
    prop::collection::vec(arb_type(depth), 0..6)
        .prop_map(|fields| Schema { fields })
        .boxed()
}

fn arb_bytes() -> BoxedStrategy<Vec<u8>> {
    prop_oneof![
        8 => prop::collection::vec(any::<u8>(), 0..40),
        // Sometimes long enough to be aligned (ALIGN_MIN = 64 KiB).
        1 => (65_530usize..66_000, any::<u8>()).prop_map(|(n, b)| vec![b; n]),
    ]
    .boxed()
}

fn arb_fields(schema: &Schema) -> BoxedStrategy<Vec<Value>> {
    let parts: Vec<BoxedStrategy<Value>> = schema
        .fields
        .iter()
        .map(|t| {
            let present: BoxedStrategy<Value> = match t {
                Type::Bool => any::<bool>().prop_map(Value::Bool).boxed(),
                Type::U64 => any::<u64>().prop_map(Value::U64).boxed(),
                Type::Bytes => arb_bytes().prop_map(Value::Bytes).boxed(),
                Type::Str => ".{0,12}".prop_map(Value::Str).boxed(),
                Type::Message(s) => arb_fields(s).prop_map(Value::Message).boxed(),
                Type::ListBool => prop::collection::vec(any::<bool>(), 0..5)
                    .prop_map(Value::ListBool)
                    .boxed(),
                Type::ListU64 => prop::collection::vec(any::<u64>(), 0..5)
                    .prop_map(Value::ListU64)
                    .boxed(),
                Type::ListBytes => prop::collection::vec(arb_bytes(), 0..4)
                    .prop_map(Value::ListBytes)
                    .boxed(),
                Type::ListStr => prop::collection::vec(".{0,6}", 0..4)
                    .prop_map(Value::ListStr)
                    .boxed(),
                Type::ListMessage(s) => prop::collection::vec(arb_fields(s), 0..3)
                    .prop_map(Value::ListMessage)
                    .boxed(),
            };
            prop_oneof![1 => Just(Value::Absent), 4 => present].boxed()
        })
        .collect();
    parts.prop_map(|v| v).boxed()
}

fn arb_doc() -> impl Strategy<Value = (Schema, Vec<Value>)> {
    arb_schema(3).prop_flat_map(|s| {
        let f = arb_fields(&s);
        (Just(s), f)
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    #[test]
    fn encode_decode_round_trip((schema, value) in arb_doc()) {
        let bytes = encode(&value, WriterOptions::default()).unwrap();
        prop_assert_eq!(&decode(&bytes, &schema).unwrap(), &value);
        prop_assert!(is_canonical(&bytes, &schema).unwrap());
        // Deterministic: a second encode gives the same bytes and id.
        let again = encode(&value, WriterOptions::default()).unwrap();
        prop_assert_eq!(sese::id(&bytes), sese::id(&again));
    }

    #[test]
    fn aligned_values_start_on_blocks((_schema, value) in arb_doc()) {
        let bytes = encode(&value, WriterOptions::default()).unwrap();
        let root = Message::root(&bytes).unwrap();
        for (i, v) in value.iter().enumerate() {
            if let Value::Bytes(b) = v {
                let (s, e) = root.bytes_range(i).unwrap().unwrap();
                prop_assert_eq!((e - s) as usize, b.len());
                if b.len() >= sese::ALIGN_MIN {
                    prop_assert_eq!(s % sese::BLOCK_SIZE as u64, 0);
                }
            }
        }
    }

    /// Any one-byte change, cut or extension of a valid file never panics the reader.
    #[test]
    fn mutated_files_never_panic((schema, value) in arb_doc(), pos in any::<prop::sample::Index>(), byte in any::<u8>(), cut in any::<prop::sample::Index>()) {
        let bytes = encode(&value, WriterOptions::default()).unwrap();
        let mut m = bytes.clone();
        if !m.is_empty() {
            let i = pos.index(m.len());
            m[i] = byte;
        }
        let _ = decode(&m, &schema);
        let c = cut.index(bytes.len() + 1);
        let _ = decode(&bytes[..c], &schema);
        let _ = decode(&bytes[c..], &schema);
        let mut ext = bytes.clone();
        ext.extend_from_slice(&bytes);
        let _ = decode(&ext, &schema);
    }

    /// Random bytes never panic the reader, with any schema.
    #[test]
    fn random_bytes_never_panic(schema in arb_schema(3), mut bytes in prop::collection::vec(any::<u8>(), 0..300), with_trailer in any::<bool>(), footer in any::<u64>()) {
        if with_trailer {
            // Give it a valid magic so that the reader goes past the trailer check.
            let n = bytes.len() as u64;
            bytes.extend((footer % (n + 1)).to_le_bytes());
            bytes.extend(sese::MAGIC.to_le_bytes());
        }
        let _ = decode(&bytes, &schema);
        let _ = is_canonical(&bytes, &schema);
    }
}

#[test]
fn reader_gives_slices_of_the_input() {
    let bytes = encode(&common::vectors()[1].1, WriterOptions::default()).unwrap();
    let root = Message::root(&bytes).unwrap();
    let s = root.str(3).unwrap().unwrap();
    let base = bytes.as_ptr() as usize;
    let p = s.as_ptr() as usize;
    assert!(p >= base && p < base + bytes.len(), "zero copy");
    assert_eq!(root.bool(0).unwrap(), Some(true));
    assert_eq!(root.u64(1).unwrap(), Some(42));
    assert_eq!(root.u64(2).unwrap(), None);
    assert_eq!(root.bytes(4).unwrap(), Some(&[1u8, 2, 3][..]));
    // Past the footer: absent, not an error.
    assert_eq!(root.u64(99).unwrap(), None);
}

#[test]
fn nested_offsets_are_absolute_in_the_file() {
    let bytes = encode(&common::vectors()[2].1, WriterOptions::default()).unwrap();
    let root = Message::root(&bytes).unwrap();
    let child = root.message(1).unwrap().unwrap();
    // The child's first field is right after the child's length prefix.
    let at = root.offset(1).unwrap().unwrap();
    assert_eq!(child.start(), at + 8);
    assert_eq!(child.offset(0).unwrap(), Some(at + 8));
    assert_eq!(child.u64(0).unwrap(), Some(7));
    assert_eq!(child.str(1).unwrap(), Some("inner"));
    let l = root.list(4).unwrap().unwrap();
    assert_eq!(l.count(), 2);
    assert_eq!(l.message(1).unwrap().u64(0).unwrap(), Some(2));
}

#[test]
fn mmap_reads_the_same() {
    let bytes = encode(&common::vectors()[2].1, WriterOptions::default()).unwrap();
    let dir = std::env::temp_dir().join(format!("sese-mmap-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let p = dir.join("x.sese");
    std::fs::write(&p, &bytes).unwrap();
    let m = sese::mmap::MappedFile::open(&p).unwrap();
    assert_eq!(m.bytes(), &bytes[..]);
    let schema = common::schema_for("nested");
    assert_eq!(decode(m.bytes(), &schema).unwrap(), common::vectors()[2].1);
    std::fs::write(dir.join("empty"), b"").unwrap();
    let e = sese::mmap::MappedFile::open(dir.join("empty")).unwrap();
    assert!(Message::root(e.bytes()).is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn writer_refuses_wrong_use() {
    let mut w = sese::Writer::default();
    w.begin(2).unwrap();
    w.u64(1, 5).unwrap();
    assert!(w.u64(0, 5).is_err(), "descending index");
    assert!(w.u64(2, 5).is_err(), "past the footer");
    let mut w = sese::Writer::default();
    w.begin(1).unwrap();
    w.begin_list(0, 2).unwrap();
    w.element_str("a").unwrap();
    assert!(w.end().is_err(), "too few elements");
    let w = sese::Writer::default();
    assert!(w.finish().is_err());
}

#[test]
fn blocks_cover_the_file() {
    let bytes = encode(&common::vectors()[3].1, WriterOptions::default()).unwrap();
    let h = sese::blocks::block_hashes(&bytes, sese::BLOCK_SIZE);
    assert_eq!(
        h.len() as u64,
        sese::blocks::block_count(bytes.len() as u64, sese::BLOCK_SIZE as u64)
    );
    let (s, e) = sese::blocks::block_range(
        bytes.len() as u64,
        sese::BLOCK_SIZE as u64,
        h.len() as u64 - 1,
    )
    .unwrap();
    assert_eq!(e, bytes.len() as u64);
    assert_eq!(
        sese::blocks::block_hash(&bytes[s as usize..]),
        h[h.len() - 1]
    );
    assert!(
        sese::blocks::block_range(bytes.len() as u64, sese::BLOCK_SIZE as u64, h.len() as u64)
            .is_none()
    );
}

/// The same large bytes value, after a field of another size, gives the same blocks:
/// alignment makes the blocks of a large file independent of what is before it.
#[test]
fn alignment_keeps_blocks_after_a_shift() {
    let big: Vec<u8> = (0..100_000u32).map(|i| (i * 7 % 256) as u8).collect();
    let a = encode(
        &[Value::Str("short".into()), Value::Bytes(big.clone())],
        WriterOptions::default(),
    )
    .unwrap();
    let b = encode(
        &[
            Value::Str("a longer string, other size".into()),
            Value::Bytes(big.clone()),
        ],
        WriterOptions::default(),
    )
    .unwrap();
    let ha: std::collections::HashSet<_> = sese::blocks::block_hashes(&a, sese::BLOCK_SIZE)
        .into_iter()
        .collect();
    let hb = sese::blocks::block_hashes(&b, sese::BLOCK_SIZE);
    let shared = hb.iter().filter(|h| ha.contains(*h)).count();
    // 100_000 bytes = 6 full blocks + a tail block: all 7 are shared. The footer and
    // trailer block is also equal (same offsets: the padding absorbed the shift).
    assert_eq!(shared, 8);
    // Without alignment, nothing is shared after the shift.
    let off = WriterOptions {
        align_min: u64::MAX,
        ..WriterOptions::default()
    };
    let a2 = encode(
        &[Value::Str("short".into()), Value::Bytes(big.clone())],
        off,
    )
    .unwrap();
    let b2 = encode(
        &[
            Value::Str("a longer string, other size".into()),
            Value::Bytes(big),
        ],
        off,
    )
    .unwrap();
    let ha2: std::collections::HashSet<_> = sese::blocks::block_hashes(&a2, sese::BLOCK_SIZE)
        .into_iter()
        .collect();
    assert_eq!(
        sese::blocks::block_hashes(&b2, sese::BLOCK_SIZE)
            .iter()
            .filter(|h| ha2.contains(*h))
            .count(),
        0
    );
}
