//! Proto Service <-> SeSe: round trips that keep the proto bytes (and so the Celaut id),
//! the zero-copy filesystem walk, refusals, and malformed input.

use proptest::prelude::*;
use prost::Message as _;
use prost_reflect::{DynamicMessage, Value};
use sese_celaut::*;

/// A file tree for tests.
#[derive(Clone, Debug)]
enum Node {
    File(Vec<u8>),
    Link(String),
    Dir(Vec<(String, Node)>),
}

fn xattrs(mode: u32) -> Vec<Value> {
    [
        "mode",
        "uid",
        "gid",
        "mtime_ns",
        "device.major",
        "device.minor",
        "device.is_block",
    ]
    .iter()
    .map(|k| {
        let mut kv = DynamicMessage::new(descriptor("celaut.BytesKeyValue").unwrap());
        kv.set_field_by_name("key", Value::String((*k).into()));
        let v = if *k == "mode" {
            mode.to_string()
        } else {
            "0".into()
        };
        kv.set_field_by_name("value", Value::Bytes(v.into_bytes().into()));
        Value::Message(kv)
    })
    .collect()
}

fn filesystem(entries: &[(String, Node)]) -> DynamicMessage {
    let mut fs = DynamicMessage::new(descriptor(FILESYSTEM).unwrap());
    let branch_desc = descriptor("celaut.Service.Container.Filesystem.ItemBranch").unwrap();
    let mut branches = Vec::new();
    for (name, node) in entries {
        let mut b = DynamicMessage::new(branch_desc.clone());
        b.set_field_by_name("name", Value::String(name.clone()));
        match node {
            Node::File(data) => b.set_field_by_name("file", Value::Bytes(data.clone().into())),
            Node::Link(src) => {
                let mut l = DynamicMessage::new(
                    descriptor("celaut.Service.Container.Filesystem.ItemBranch.Link").unwrap(),
                );
                l.set_field_by_name("src", Value::String(src.clone()));
                l.set_field_by_name("dst", Value::String(format!("/{name}")));
                b.set_field_by_name("link", Value::Message(l));
            }
            Node::Dir(children) => {
                b.set_field_by_name("filesystem", Value::Message(filesystem(children)))
            }
        }
        b.set_field_by_name("xattrs", Value::List(xattrs(0o100644)));
        branches.push(Value::Message(b));
    }
    fs.set_field_by_name("branch", Value::List(branches));
    fs
}

fn service_text() -> &'static str {
    r#"
    prose: "a test service"
    container {
      architecture { tags: "linux/amd64" prose: "x86" formal: "bits=64" }
      init { entry_path: "app" entry_path: "run" }
      resources { at_init { mem_limit: 134217728 } at_most { mem_limit: 268435456 benchmark { key: "cpu" value: 0 } } }
      config_declaration { path: "__config__" format {} }
      environment_variables { key: "MODE" value { tags: "text" } }
      notice_declaration { path: "dev" path: "hvc0" system { tags: "virtio-console" } format { tags: "nodo-notice" } }
      devices { architecture { tags: "nvidia" } attach: ATTACH_ON_REQUEST attach: ATTACH_AT_START }
    }
    api { slot { port: 8080 transport { tags: "tcp" } protocol_stack { tags: "http" } protocol_stack { tags: "chat" formal: "rev=6" } } }
    network { tags: "*" prose: "all" }
    initial_environment_workload {
      any_of { workloads { count: 2 dependency { hash { type: "\x01" value: "\xff\x00" } is_completed: true
        service { prose: "child" container { architecture { tags: "linux/amd64" } } } } } }
    }
    "#
}

fn service(entries: &[(String, Node)]) -> Vec<u8> {
    let mut s =
        DynamicMessage::parse_text_format(descriptor(SERVICE).unwrap(), service_text()).unwrap();
    let fs = filesystem(entries).encode_to_vec();
    let Value::Message(c) = s.get_field_by_name_mut("container").unwrap() else {
        panic!()
    };
    c.set_field_by_name("filesystem", Value::Bytes(fs.into()));
    s.encode_to_vec()
}

fn sample_tree() -> Vec<(String, Node)> {
    vec![
        (
            "bin".into(),
            Node::Dir(vec![
                (
                    "app".into(),
                    Node::File((0..100_000u32).map(|i| (i % 253) as u8).collect()),
                ),
                ("sh".into(), Node::Link("/bin/busybox".into())),
                ("empty".into(), Node::File(vec![])),
            ]),
        ),
        (
            "etc".into(),
            Node::Dir(vec![(
                "hosts".into(),
                Node::File(b"127.0.0.1 localhost\n".to_vec()),
            )]),
        ),
        ("big".into(), Node::File(vec![7u8; 70_000])),
    ]
}

#[test]
fn service_round_trip_keeps_the_proto_bytes_and_the_celaut_id() {
    let proto = service(&sample_tree());
    let sese = service_to_sese(&proto).unwrap();
    let back = sese_to_service(&sese).unwrap();
    assert_eq!(back, proto);
    use sha3::Digest;
    assert_eq!(
        sha3::Sha3_256::digest(&back),
        sha3::Sha3_256::digest(&proto)
    );
    // Deterministic: the same service gives the same SeSe bytes and id.
    assert_eq!(service_to_sese(&back).unwrap(), sese);
}

#[test]
fn walk_gives_the_files_with_no_copy() {
    let sese = service_to_sese(&service(&sample_tree())).unwrap();
    let root = sese::Message::root(&sese).unwrap();
    let mut seen = Vec::new();
    walk_filesystem(&root, |path, e| {
        if let Entry::File(d) = e {
            let p = d.as_ptr() as usize;
            assert!(p >= sese.as_ptr() as usize && p <= sese.as_ptr() as usize + sese.len());
        }
        seen.push((
            path.to_string(),
            match e {
                Entry::File(d) => format!("file {}", d.len()),
                Entry::Link(s) => format!("link {s}"),
                Entry::Dir => "dir".into(),
                Entry::Empty => "empty".into(),
            },
        ));
    })
    .unwrap();
    assert_eq!(
        seen,
        vec![
            ("bin".into(), "dir".into()),
            ("bin/app".into(), "file 100000".into()),
            ("bin/sh".into(), "link /bin/busybox".into()),
            ("bin/empty".into(), "file 0".into()),
            ("etc".into(), "dir".into()),
            ("etc/hosts".into(), "file 20".into()),
            ("big".into(), "file 70000".into()),
        ]
    );
}

#[test]
fn large_files_are_block_aligned() {
    let sese = service_to_sese(&service(&sample_tree())).unwrap();
    let root = sese::Message::root(&sese).unwrap();
    let mut aligned = 0;
    walk_filesystem(&root, |_, e| {
        if let Entry::File(d) = e {
            if d.len() >= sese::ALIGN_MIN {
                assert_eq!(
                    (d.as_ptr() as usize - sese.as_ptr() as usize) % sese::BLOCK_SIZE,
                    0
                );
                aligned += 1;
            }
        }
    })
    .unwrap();
    assert_eq!(aligned, 2);
}

#[test]
fn scalars_of_every_kind_round_trip() {
    let d = descriptor("celaut.EstimatedCost").unwrap();
    let m = DynamicMessage::parse_text_format(d.clone(),
        "maintenance_seconds_loop: -5 price_bounded: true price_volatility: -1.5 volatility_period_seconds: 2147483647 closes_at: -9223372036854775808 on_close: 1").unwrap();
    let proto = m.encode_to_vec();
    let sese = proto_to_sese(&d, &proto).unwrap();
    assert_eq!(sese_to_proto(&d, &sese).unwrap(), proto);
    let root = sese::Message::root(&sese).unwrap();
    assert_eq!(
        root.u64(3).unwrap(),
        Some((-5i64) as u64),
        "int32 sign-extended"
    );
    assert_eq!(
        root.u64(5).unwrap(),
        Some(u64::from((-1.5f32).to_bits())),
        "float bits"
    );
    assert_eq!(root.u64(0).unwrap(), None, "unset message is absent");
    // Implicit-presence scalars are written also at their default.
    let empty = proto_to_sese(&d, &[]).unwrap();
    assert_eq!(
        sese::Message::root(&empty).unwrap().u64(3).unwrap(),
        Some(0)
    );
    assert_eq!(sese_to_proto(&d, &empty).unwrap(), Vec::<u8>::new());
    // Sysresources: field 99 -> footer of 99 entries; optional uint64 with presence.
    let d = descriptor("celaut.Sysresources").unwrap();
    let m = DynamicMessage::parse_text_format(
        d.clone(),
        "mem_limit: 0 benchmark { key: \"a\" value: 3 }",
    )
    .unwrap();
    let proto = m.encode_to_vec();
    let sese = proto_to_sese(&d, &proto).unwrap();
    let root = sese::Message::root(&sese).unwrap();
    assert_eq!(root.field_count(), 99);
    assert_eq!(
        root.u64(3).unwrap(),
        Some(0),
        "optional set to 0 is present"
    );
    assert_eq!(root.u64(0).unwrap(), None, "optional unset is absent");
    assert_eq!(sese_to_proto(&d, &sese).unwrap(), proto);
}

#[test]
fn refuses_what_it_cannot_keep() {
    // Unknown field (number 1000) in a Service.
    let mut proto = service(&[]);
    proto.extend([0xc0, 0x3e, 0x01]); // field 1000, varint 1
    assert!(matches!(service_to_sese(&proto), Err(Error::Lossy(_))));
    // A filesystem whose proto bytes are not canonical (two fields out of order).
    let mut s =
        DynamicMessage::parse_text_format(descriptor(SERVICE).unwrap(), "prose: \"x\"").unwrap();
    let mut c = DynamicMessage::new(descriptor("celaut.Service.Container").unwrap());
    // Filesystem with xattrs (field 2) before branch (field 1).
    let fs = vec![0x12, 0x00, 0x0a, 0x00];
    c.set_field_by_name("filesystem", Value::Bytes(fs.into()));
    s.set_field_by_name("container", Value::Message(c));
    assert!(matches!(
        service_to_sese(&s.encode_to_vec()),
        Err(Error::Lossy(_))
    ));
    // A uint32 out of range in SeSe.
    let d = descriptor("celaut.Service.Workload").unwrap();
    let mut w = sese::Writer::default();
    w.begin(2).unwrap();
    w.u64(0, 1u64 << 40).unwrap();
    w.end().unwrap();
    assert!(matches!(
        sese_to_proto(&d, &w.finish().unwrap()),
        Err(Error::Lossy(_))
    ));
    // Two members of the ItemBranch oneof.
    let d = descriptor("celaut.Service.Container.Filesystem.ItemBranch").unwrap();
    let mut w = sese::Writer::default();
    w.begin(5).unwrap();
    w.str(0, "x").unwrap();
    w.bytes(1, b"data").unwrap();
    w.begin_message(2, 2).unwrap();
    w.end().unwrap();
    w.end().unwrap();
    assert!(matches!(
        sese_to_proto(&d, &w.finish().unwrap()),
        Err(Error::Lossy(_))
    ));
}

fn arb_tree(depth: u32) -> BoxedStrategy<Vec<(String, Node)>> {
    let file = prop_oneof![
        6 => prop::collection::vec(any::<u8>(), 0..64),
        1 => (60_000usize..70_000, any::<u8>()).prop_map(|(n, b)| (0..n).map(|i| b.wrapping_add(i as u8)).collect()),
    ].prop_map(Node::File);
    let link = "[a-z/]{1,12}".prop_map(Node::Link);
    let leaf = prop_oneof![4 => file, 1 => link];
    let node = if depth == 0 {
        leaf.boxed()
    } else {
        prop_oneof![3 => leaf, 1 => arb_tree(depth - 1).prop_map(Node::Dir)].boxed()
    };
    prop::collection::vec(("[a-z]{1,8}", node), 0..5).boxed()
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 128, ..ProptestConfig::default() })]

    #[test]
    fn random_trees_round_trip(tree in arb_tree(3)) {
        let proto = service(&tree);
        let sese = service_to_sese(&proto).unwrap();
        prop_assert_eq!(sese_to_service(&sese).unwrap(), proto);
    }

    #[test]
    fn mutated_service_never_panics(pos in any::<prop::sample::Index>(), byte in any::<u8>(), cut in any::<prop::sample::Index>()) {
        let sese = service_to_sese(&service(&sample_tree()[..2])).unwrap();
        let mut m = sese.clone();
        let i = pos.index(m.len());
        m[i] = byte;
        let _ = sese_to_service(&m);
        if let Ok(root) = sese::Message::root(&m) {
            let _ = walk_filesystem(&root, |_, _| {});
        }
        let _ = sese_to_service(&sese[..cut.index(sese.len())]);
    }
}
