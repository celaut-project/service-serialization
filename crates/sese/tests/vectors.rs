//! Golden vectors: the canonical bytes of fixed values, committed in tests/vectors/.
//! Regenerate with `SESE_REGEN=1 cargo test -p sese --test vectors` (then review the diff).
mod common;

use sese::value::{decode, encode, is_canonical};
use sese::WriterOptions;

fn dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

#[test]
fn vectors_match_the_committed_bytes() {
    let regen = std::env::var_os("SESE_REGEN").is_some();
    let mut ids = String::new();
    for (name, value) in common::vectors() {
        let bytes = encode(&value, WriterOptions::default()).unwrap();
        let path = dir().join(format!("{name}.sese"));
        if regen {
            std::fs::write(&path, &bytes).unwrap();
        }
        let committed = std::fs::read(&path).unwrap();
        assert_eq!(bytes, committed, "vector {name}");
        let schema = common::schema_for(name);
        assert_eq!(decode(&committed, &schema).unwrap(), value, "decode {name}");
        assert!(
            is_canonical(&committed, &schema).unwrap(),
            "canonical {name}"
        );
        ids.push_str(&format!(
            "{} {name}.sese\n",
            hex::encode(sese::id(&committed))
        ));
    }
    let ids_path = dir().join("ids.txt");
    if regen {
        std::fs::write(&ids_path, &ids).unwrap();
    }
    assert_eq!(std::fs::read_to_string(ids_path).unwrap(), ids);
}

/// The byte layout of the `scalars` vector, written out by hand from SPEC.md.
#[test]
fn scalars_layout_by_hand() {
    let mut want = Vec::new();
    want.push(1u8); // field 0 bool at 0
    want.extend(42u64.to_le_bytes()); // field 1 u64 at 1
    want.extend(2u64.to_le_bytes()); // field 3 string at 9
    want.extend(b"hi");
    want.extend(3u64.to_le_bytes()); // field 4 bytes at 19
    want.extend([1, 2, 3]);
    // footer at 30
    for o in [0u64, 1, u64::MAX, 9, 19] {
        want.extend(o.to_le_bytes());
    }
    want.extend(30u64.to_le_bytes());
    want.extend(b"PM_MV_FF"); // MAGIC 0x46465F564D5F4D50, little-endian
    let got = std::fs::read(dir().join("scalars.sese")).unwrap();
    assert_eq!(got, want);
}

#[test]
fn empty_message_is_its_trailer() {
    let got = std::fs::read(dir().join("empty.sese")).unwrap();
    let mut want = 0u64.to_le_bytes().to_vec();
    want.extend(sese::MAGIC.to_le_bytes());
    assert_eq!(got, want);
}

#[test]
fn aligned_bytes_start_on_a_block_and_pad_to_one() {
    let got = std::fs::read(dir().join("aligned.sese")).unwrap();
    let root = sese::Message::root(&got).unwrap();
    let (start, end) = root.bytes_range(1).unwrap().unwrap();
    assert_eq!(start % sese::BLOCK_SIZE as u64, 0);
    assert_eq!(end - start, 70_000);
    // The next field starts at the next block boundary.
    let next = root.offset(2).unwrap().unwrap();
    assert_eq!(
        next,
        end.div_ceil(sese::BLOCK_SIZE as u64) * sese::BLOCK_SIZE as u64
    );
    // Padding is zero.
    assert!(got[end as usize..next as usize].iter().all(|b| *b == 0));
    assert!(got[9..(start - 8) as usize].iter().all(|b| *b == 0));
}
