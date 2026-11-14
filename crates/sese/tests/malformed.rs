//! Malformed and truncated inputs: each gives an error, never a panic.
mod common;

use sese::value::{decode, encode};
use sese::{Error, Message, WriterOptions, MAGIC};

fn trailer(footer: u64) -> Vec<u8> {
    let mut v = footer.to_le_bytes().to_vec();
    v.extend(MAGIC.to_le_bytes());
    v
}

fn scalars() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors/scalars.sese"),
    )
    .unwrap()
}

#[test]
fn too_short() {
    for n in 0..16 {
        assert!(
            matches!(Message::root(&vec![0u8; n]), Err(Error::TooShort { .. })),
            "{n}"
        );
    }
}

#[test]
fn bad_magic() {
    let mut f = scalars();
    let n = f.len();
    f[n - 1] ^= 1;
    assert!(matches!(Message::root(&f), Err(Error::BadMagic { .. })));
}

#[test]
fn footer_outside_or_not_aligned() {
    // Footer offset past the trailer.
    assert!(matches!(
        Message::root(&trailer(1)),
        Err(Error::BadFooter { .. })
    ));
    // Footer is not a whole number of u64.
    let mut f = vec![0u8; 3];
    f.extend(trailer(0));
    assert!(matches!(Message::root(&f), Err(Error::BadFooter { .. })));
    // u64::MAX
    assert!(matches!(
        Message::root(&trailer(u64::MAX)),
        Err(Error::BadFooter { .. })
    ));
}

#[test]
fn field_offset_outside_of_the_body() {
    // One field whose offset points into the footer itself.
    let mut f = 0u64.to_le_bytes().to_vec(); // footer at 0: entry = 0, but the body is empty
    f.extend(trailer(0));
    let m = Message::root(&f).unwrap();
    assert!(matches!(m.u64(0), Err(Error::OutOfBounds { .. })));
}

#[test]
fn length_past_the_body() {
    let mut f = scalars();
    // field 3 (string "hi") is at 9: set its length to 1000.
    f[9..17].copy_from_slice(&1000u64.to_le_bytes());
    let m = Message::root(&f).unwrap();
    assert!(matches!(m.str(3), Err(Error::OutOfBounds { .. })));
    // A length that overflows u64 when added.
    f[9..17].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(matches!(
        Message::root(&f).unwrap().str(3),
        Err(Error::OutOfBounds { .. })
    ));
}

#[test]
fn bad_bool_and_utf8() {
    let mut f = scalars();
    f[0] = 2;
    assert!(matches!(
        Message::root(&f).unwrap().bool(0),
        Err(Error::BadBool { offset: 0 })
    ));
    let mut f = scalars();
    f[17] = 0xff; // "hi" -> invalid UTF-8
    assert!(matches!(
        Message::root(&f).unwrap().str(3),
        Err(Error::BadUtf8 { .. })
    ));
}

#[test]
fn every_truncation_of_a_vector_is_refused() {
    for (name, value) in common::vectors() {
        let bytes = encode(&value, WriterOptions::default()).unwrap();
        let schema = common::schema_for(name);
        for cut in 0..bytes.len() {
            // A prefix has no valid trailer at its end (almost always); it must never
            // decode to the full value.
            if let Ok(v) = decode(&bytes[..cut], &schema) {
                assert_ne!(v, value, "{name} cut at {cut}");
            }
        }
    }
}

#[test]
fn huge_list_count_does_not_allocate() {
    // A list of strings that claims u64::MAX / 8 elements.
    let mut f = (u64::MAX / 8).to_le_bytes().to_vec();
    f.extend(0u64.to_le_bytes()); // footer: field 0 at 0
    f.extend(trailer(8));
    let schema = sese::value::Schema {
        fields: vec![sese::value::Type::ListStr],
    };
    assert!(matches!(
        decode(&f, &schema),
        Err(Error::OutOfBounds { .. })
    ));
    let schema = sese::value::Schema {
        fields: vec![sese::value::Type::ListU64],
    };
    assert!(matches!(
        decode(&f, &schema),
        Err(Error::OutOfBounds { .. })
    ));
    let m = Message::root(&f).unwrap();
    let l = m.list(0).unwrap().unwrap();
    assert!(l.str(u64::MAX - 1).is_err());
}

#[test]
fn nested_message_cannot_escape_its_parent() {
    let bytes = encode(&common::vectors()[2].1, WriterOptions::default()).unwrap();
    let root = Message::root(&bytes).unwrap();
    let at = root.offset(1).unwrap().unwrap() as usize;
    let mut f = bytes.clone();
    // Make the child claim the whole rest of the file.
    let rest = (bytes.len() - at - 8) as u64;
    f[at..at + 8].copy_from_slice(&rest.to_le_bytes());
    assert!(Message::root(&f).unwrap().message(1).is_err());
}

#[test]
fn self_nesting_is_bounded() {
    // Deep nesting: each level a message with one message field. Decoding past
    // MAX_DEPTH is refused, not a stack overflow.
    let mut v = vec![];
    for _ in 0..(sese::MAX_DEPTH + 5) {
        v = vec![sese::value::Value::Message(v)];
    }
    // The writer also refuses it.
    assert!(matches!(
        encode(&v, WriterOptions::default()),
        Err(Error::TooDeep)
    ));
}
