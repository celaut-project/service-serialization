//! Package tool for the benchmark.
//!
//! ```text
//! pkgtool extract <in.celaut.bee> <dir>        -> dir/meta.bin, dir/service.pb (expanded)
//! pkgtool mkbee <dir> <out.celaut.bee> [min]   -> the .bee that `nodo export` writes for it
//! pkgtool tosese <dir> <out.sese>              -> canonical SeSe of service.pb, with checks
//! pkgtool v2 <dir> <outdir>                    -> a slightly changed version (see `make_v2`)
//! pkgtool stats <a.sese> <b.sese>              -> blocks shared between two SeSe files
//! pkgtool blocks <a.celaut.bee> <b.celaut.bee> -> bee-rpc blocks shared between two packages
//! ```
//!
//! `mkbee` rebuilds the package as a multiblock directory (the form of the node's store)
//! and reads it back with `bee_rpc::store::Store` (block depth 2), as the node does for
//! an export. Blocks: the filesystem (depth 1) and each file of at least `min` bytes
//! (depth 2). `mkbee` on the extract of a real export gives that export byte for byte.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::Instant;

use bee_rpc::bee_file::{read_bee, write_bee};
use bee_rpc::frame::{ParseOptions, Parser};
use bee_rpc::store::Store;
use prost::Message as _;
use prost_reflect::{DynamicMessage, Value};
use sha3::{Digest, Sha3_256};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let arg = |i: usize| a.get(i).cloned().unwrap_or_else(|| usage());
    match a.get(1).map(String::as_str) {
        Some("extract") => extract(&arg(2), &arg(3)),
        Some("mkbee") => mkbee(
            &arg(2),
            &arg(3),
            a.get(4).and_then(|s| s.parse().ok()).unwrap_or(0),
        ),
        Some("tosese") => tosese(&arg(2), &arg(3)),
        Some("v2") => v2(&arg(2), &arg(3)),
        Some("stats") => stats(&arg(2), &arg(3)),
        Some("blocks") => bee_blocks(&arg(2), &arg(3)),
        _ => usage(),
    }
}

fn usage() -> ! {
    eprintln!("usage: pkgtool extract|mkbee|tosese|v2|stats|blocks ...");
    std::process::exit(2)
}

fn sha3(b: &[u8]) -> Vec<u8> {
    Sha3_256::digest(b).to_vec()
}

/// Parts of a .bee and the ids of its blocks (by depth).
fn parse_bee(path: &str) -> (Vec<bee_rpc::frame::Part>, Vec<(usize, Vec<u8>)>) {
    let mut parser = Parser::new(ParseOptions::default());
    let mut parts = Vec::new();
    for b in read_bee(BufReader::new(File::open(path).expect("open"))) {
        parts.extend(parser.push(b.expect("record")).expect("parse"));
    }
    parts.extend(parser.finish().expect("finish"));
    let blocks = parser
        .block_starts()
        .iter()
        .map(|s| (s.depth, s.id.clone()))
        .collect();
    (parts, blocks)
}

fn extract(input: &str, dir: &str) {
    let (parts, blocks) = parse_bee(input);
    fs::create_dir_all(dir).expect("mkdir");
    for p in &parts {
        let name = if p.index == 1 {
            "meta.bin"
        } else {
            "service.pb"
        };
        fs::write(Path::new(dir).join(name), &p.data).expect("write");
        println!(
            "part {} -> {name}: {} bytes, sha3 {}",
            p.index,
            p.data.len(),
            hex::encode(sha3(&p.data))
        );
    }
    // Which files are blocks: the smallest block file and the largest file that is not.
    let ids: HashSet<Vec<u8>> = blocks
        .iter()
        .filter(|(d, _)| *d == 2)
        .map(|(_, id)| id.clone())
        .collect();
    let service = parts.iter().find(|p| p.index == 2).expect("service part");
    let mut min_block = u64::MAX;
    let mut max_plain = 0u64;
    for_each_file(&service.data, |_, data| {
        if ids.contains(&sha3(data)) {
            min_block = min_block.min(data.len() as u64);
        } else {
            max_plain = max_plain.max(data.len() as u64);
        }
    });
    println!("blocks: {} (depth 2: {}); smallest block file {min_block} B, largest plain file {max_plain} B", blocks.len(), ids.len());
}

// ---------------------------------------------------------------------------------------
// The protobuf wire walk: positions of the length varints in the expanded form.

fn varint(b: &[u8], at: usize) -> (u64, usize) {
    let mut v = 0u64;
    let mut i = at;
    let mut shift = 0;
    loop {
        let x = b[i];
        v |= u64::from(x & 0x7f) << shift;
        i += 1;
        if x < 0x80 {
            return (v, i);
        }
        shift += 7;
    }
}

/// The LEN fields of the message in `b[start..end]`: (field number, position of the length
/// varint, content start, content end).
fn len_fields(b: &[u8], start: usize, end: usize) -> Vec<(u64, usize, usize, usize)> {
    let mut out = Vec::new();
    let mut i = start;
    while i < end {
        let (key, j) = varint(b, i);
        let (num, wt) = (key >> 3, key & 7);
        match wt {
            0 => i = varint(b, j).1,
            1 => i = j + 8,
            5 => i = j + 4,
            2 => {
                let (len, k) = varint(b, j);
                out.push((num, j, k, k + len as usize));
                i = k + len as usize;
            }
            _ => panic!("wire type {wt}"),
        }
    }
    out
}

/// The content range of Service.container.filesystem and the positions of the two length
/// varints that hold it.
fn filesystem_range(service: &[u8]) -> (usize, usize, Vec<u64>) {
    let c = len_fields(service, 0, service.len())
        .into_iter()
        .find(|f| f.0 == 2)
        .expect("container");
    let f = len_fields(service, c.2, c.3)
        .into_iter()
        .find(|f| f.0 == 2)
        .expect("filesystem");
    (f.2, f.3, vec![c.1 as u64, f.1 as u64])
}

/// `f(path, content)` for each file of the service.
fn for_each_file(service: &[u8], mut f: impl FnMut(&str, &[u8])) {
    let (s, e, _) = filesystem_range(service);
    walk_fs(service, s, e, "", &mut |p, _, _, _, data| f(p, data));
}

/// The callback of a filesystem walk: `(path, plp relative to the filesystem start, file
/// start, file end, data)`.
type FileFn<'f> = dyn FnMut(&str, Vec<u64>, usize, usize, &[u8]) + 'f;

/// Walk a Filesystem in `b[s..e]`: `f(path, plp-relative-to-fs-start, file start, file end, data)`.
fn walk_fs(b: &[u8], s: usize, e: usize, prefix: &str, f: &mut FileFn<'_>) {
    walk_fs_inner(b, s, s, e, prefix, Vec::new(), f);
}

fn walk_fs_inner(
    b: &[u8],
    base: usize,
    s: usize,
    e: usize,
    prefix: &str,
    plp: Vec<u64>,
    f: &mut FileFn<'_>,
) {
    for (num, lenpos, bs, be) in len_fields(b, s, e) {
        if num != 1 {
            continue;
        }
        let fields = len_fields(b, bs, be);
        let name = fields
            .iter()
            .find(|x| x.0 == 1)
            .map(|x| String::from_utf8_lossy(&b[x.2..x.3]).to_string())
            .unwrap_or_default();
        let path = format!("{prefix}/{name}");
        for (n2, lp2, cs, ce) in &fields {
            let mut p = plp.clone();
            p.push((lenpos - base) as u64);
            p.push((*lp2 - base) as u64);
            if *n2 == 2 {
                f(&path, p, *cs, *ce, &b[*cs..*ce]);
            } else if *n2 == 4 {
                walk_fs_inner(b, base, *cs, *ce, &path, p, f);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// mkbee

fn write_json(dir: &Path, entries: &[String]) {
    fs::write(dir.join("_.json"), format!("[{}]", entries.join(", "))).expect("_.json");
}

fn mkbee(dir: &str, out: &str, min: usize) {
    let min = if min == 0 { 32 * 1024 } else { min };
    let t = Instant::now();
    let dir = Path::new(dir);
    let meta = fs::read(dir.join("meta.bin")).expect("meta.bin");
    let service = fs::read(dir.join("service.pb")).expect("service.pb");
    let work = PathBuf::from(format!("{}.store", out));
    let _ = fs::remove_dir_all(&work);
    let blocks = work.join("blocks");
    fs::create_dir_all(&blocks).expect("mkdir");
    // The filesystem block: a multiblock directory with the large files as blocks.
    let (fs_s, fs_e, fs_plp) = filesystem_range(&service);
    let fs_id = hex::encode(sha3(&service[fs_s..fs_e]));
    let fs_dir = blocks.join(&fs_id);
    fs::create_dir_all(&fs_dir).expect("mkdir");
    let mut entries = Vec::new();
    let mut cursor = fs_s;
    let mut piece = 0;
    let mut n_blocks = 0;
    let mut files = Vec::new();
    walk_fs(&service, fs_s, fs_e, "", &mut |_, plp, cs, ce, data| {
        if data.len() >= min {
            files.push((plp, cs, ce));
        }
    });
    for (plp, cs, ce) in files {
        let data = &service[cs..ce];
        if cs > cursor {
            fs::write(fs_dir.join(piece.to_string()), &service[cursor..cs]).expect("piece");
            entries.push(piece.to_string());
            piece += 1;
        }
        let id = hex::encode(sha3(data));
        let path = blocks.join(&id);
        if !path.exists() {
            fs::write(&path, data).expect("block");
        }
        entries.push(format!(
            "[\"{id}\", [{}]]",
            plp.iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        n_blocks += 1;
        cursor = ce;
    }
    if fs_e > cursor {
        fs::write(fs_dir.join(piece.to_string()), &service[cursor..fs_e]).expect("piece");
        entries.push(piece.to_string());
    }
    write_json(&fs_dir, &entries);
    // The service: before the filesystem, the filesystem block, after it.
    let svc_dir = work.join("service");
    fs::create_dir_all(&svc_dir).expect("mkdir");
    fs::write(svc_dir.join("0"), &service[..fs_s]).expect("0");
    let mut e = vec![
        "0".to_string(),
        format!(
            "[\"{fs_id}\", [{}]]",
            fs_plp
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ];
    if fs_e < service.len() {
        fs::write(svc_dir.join("1"), &service[fs_e..]).expect("1");
        e.push("1".into());
    }
    write_json(&svc_dir, &e);
    fs::write(work.join("meta.bin"), &meta).expect("meta");
    let store = Store::new(&blocks).with_block_depth(2);
    let bufs = store
        .read_message(1, work.join("meta.bin"))
        .expect("meta")
        .chain(store.read_message(2, &svc_dir).expect("service"))
        .map(|b| b.expect("store read"));
    let mut w = BufWriter::new(File::create(out).expect("create"));
    write_bee(&mut w, bufs).expect("write");
    w.flush().expect("flush");
    println!(
        "{out}: {} file blocks + 1 filesystem block (min {min} B), service sha3 {} in {:.2?}",
        n_blocks,
        hex::encode(sha3(&service)),
        t.elapsed()
    );
    if std::env::var_os("PKG_KEEP_STORE").is_none() {
        let _ = fs::remove_dir_all(&work);
    } else {
        println!("store kept: {}", work.display());
    }
}

// ---------------------------------------------------------------------------------------
// tosese

fn tosese(dir: &str, out: &str) {
    let service = fs::read(Path::new(dir).join("service.pb")).expect("service.pb");
    let t = Instant::now();
    let sese = sese_celaut::service_to_sese(&service).expect("to sese");
    let enc = t.elapsed();
    fs::write(out, &sese).expect("write");
    let t = Instant::now();
    let back = sese_celaut::sese_to_service(&sese).expect("back");
    let dec = t.elapsed();
    assert_eq!(back, service, "round trip");
    let again = sese_celaut::service_to_sese(&back).expect("again");
    assert_eq!(again, sese, "deterministic");
    let root = sese::Message::root(&sese).expect("root");
    let mut files = 0u64;
    let mut fbytes = 0u64;
    let t = Instant::now();
    sese_celaut::walk_filesystem(&root, |_, e| {
        if let sese_celaut::Entry::File(d) = e {
            files += 1;
            fbytes += d.len() as u64;
        }
    })
    .expect("walk");
    let walk = t.elapsed();
    let nblocks = sese::blocks::block_count(sese.len() as u64, sese::BLOCK_SIZE as u64);
    let hashes = sese::blocks::block_hashes(&sese, sese::BLOCK_SIZE);
    let distinct: HashSet<_> = hashes.iter().collect();
    let zero = sese::blocks::block_hash(&[0u8; sese::BLOCK_SIZE]);
    println!("proto {} B -> sese {} B (+{:.2}%), encode {enc:.2?}, decode to proto {dec:.2?}, walk {files} files ({fbytes} B) {walk:.2?}",
        service.len(), sese.len(), 100.0 * (sese.len() as f64 / service.len() as f64 - 1.0));
    println!(
        "celaut id (sha3 of proto) kept: {}",
        hex::encode(sha3(&back))
    );
    println!("sese id (blake3 of file): {}", hex::encode(sese::id(&sese)));
    println!(
        "blocks {nblocks} of {} B, distinct {}, all-zero blocks {}",
        sese::BLOCK_SIZE,
        distinct.len(),
        hashes.iter().filter(|h| **h == zero).count()
    );
}

// ---------------------------------------------------------------------------------------
// v2: a slightly changed version, as a new build of the same service would be.

fn v2(dir: &str, outdir: &str) {
    let service = fs::read(Path::new(dir).join("service.pb")).expect("service.pb");
    let meta = fs::read(Path::new(dir).join("meta.bin")).expect("meta.bin");
    let desc = sese_celaut::descriptor(sese_celaut::SERVICE).expect("desc");
    let mut s = DynamicMessage::decode(desc, service.as_slice()).expect("decode");
    let Some(Value::Message(c)) = s.get_field_by_name_mut("container") else {
        panic!("container")
    };
    let fs_bytes = c
        .get_field_by_name("filesystem")
        .expect("fs")
        .as_bytes()
        .expect("bytes")
        .clone();
    let fs_desc = sese_celaut::descriptor(sese_celaut::FILESYSTEM).expect("desc");
    let mut tree = DynamicMessage::decode(fs_desc, fs_bytes.as_ref()).expect("fs");
    let mut log = Vec::new();
    change_tree(&mut tree, "", &mut log);
    c.set_field_by_name("filesystem", Value::Bytes(tree.encode_to_vec().into()));
    let out = s.encode_to_vec();
    fs::create_dir_all(outdir).expect("mkdir");
    fs::write(Path::new(outdir).join("service.pb"), &out).expect("write");
    fs::write(Path::new(outdir).join("meta.bin"), &meta).expect("write");
    for l in log {
        println!("{l}");
    }
    println!(
        "v2 service {} B (v1 {} B), sha3 {}",
        out.len(),
        service.len(),
        hex::encode(sha3(&out))
    );
}

/// The change: what a small source edit and rebuild of a service does.
/// - the largest file under /app (or the largest file) gets 100 bytes inserted in its middle;
/// - the first small file of the tree (100 B to 4 KiB, in tree order) gets a line appended,
///   so that everything after it moves (a size change early in the file);
/// - a new 300 KiB file `/app/assets/new.bin` (deterministic bytes);
/// - the mtime of the changed files changes.
fn change_tree(tree: &mut DynamicMessage, prefix: &str, log: &mut Vec<String>) {
    // Find the largest file in the tree first.
    let mut largest = (0usize, String::new());
    find_largest(tree, "", &mut largest);
    let mut first_small = None;
    find_first_small(tree, "", &mut first_small);
    edit(
        tree,
        prefix,
        &largest.1,
        first_small.as_deref().unwrap_or(""),
        log,
    );
    // New file: in the first top-level directory named "app", else at the root.
    let new_data: Vec<u8> = (0..300 * 1024u32)
        .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    let branch_desc =
        sese_celaut::descriptor("celaut.Service.Container.Filesystem.ItemBranch").expect("d");
    let mut nb = DynamicMessage::new(branch_desc);
    nb.set_field_by_name("name", Value::String("new.bin".into()));
    nb.set_field_by_name("file", Value::Bytes(new_data.into()));
    // Copy the xattrs of a sibling file so the entry has all the keys.
    let Value::List(branches) = tree.get_field_by_name_mut("branch").expect("b") else {
        panic!()
    };
    let template = branches
        .iter()
        .filter_map(Value::as_message)
        .find(|b| b.has_field_by_name("file"))
        .cloned();
    if let Some(t) = template {
        nb.set_field_by_name(
            "xattrs",
            t.get_field_by_name("xattrs").expect("x").into_owned(),
        );
    }
    branches.push(Value::Message(nb));
    log.push("added /new.bin (307200 B)".into());
}

fn find_largest(tree: &DynamicMessage, prefix: &str, best: &mut (usize, String)) {
    for b in tree
        .get_field_by_name("branch")
        .expect("b")
        .as_list()
        .expect("l")
    {
        let b = b.as_message().expect("m");
        let name = b
            .get_field_by_name("name")
            .expect("n")
            .as_str()
            .unwrap_or("")
            .to_string();
        let path = format!("{prefix}/{name}");
        if b.has_field_by_name("file") {
            let n = b
                .get_field_by_name("file")
                .expect("f")
                .as_bytes()
                .expect("b")
                .len();
            if n > best.0 {
                *best = (n, path);
            }
        } else if b.has_field_by_name("filesystem") {
            find_largest(
                b.get_field_by_name("filesystem")
                    .expect("fs")
                    .as_message()
                    .expect("m"),
                &path,
                best,
            );
        }
    }
}

fn find_first_small(tree: &DynamicMessage, prefix: &str, out: &mut Option<String>) {
    for b in tree
        .get_field_by_name("branch")
        .expect("b")
        .as_list()
        .expect("l")
    {
        if out.is_some() {
            return;
        }
        let b = b.as_message().expect("m");
        let name = b
            .get_field_by_name("name")
            .expect("n")
            .as_str()
            .unwrap_or("")
            .to_string();
        let path = format!("{prefix}/{name}");
        if b.has_field_by_name("file") {
            let n = b
                .get_field_by_name("file")
                .expect("f")
                .as_bytes()
                .expect("b")
                .len();
            if (100..=4096).contains(&n) {
                *out = Some(path);
            }
        } else if b.has_field_by_name("filesystem") {
            find_first_small(
                b.get_field_by_name("filesystem")
                    .expect("fs")
                    .as_message()
                    .expect("m"),
                &path,
                out,
            );
        }
    }
}

fn edit(
    tree: &mut DynamicMessage,
    prefix: &str,
    largest: &str,
    small: &str,
    log: &mut Vec<String>,
) {
    let Value::List(branches) = tree.get_field_by_name_mut("branch").expect("b") else {
        panic!()
    };
    for b in branches.iter_mut() {
        let Value::Message(b) = b else { continue };
        let name = b
            .get_field_by_name("name")
            .expect("n")
            .as_str()
            .unwrap_or("")
            .to_string();
        let path = format!("{prefix}/{name}");
        let mut changed = false;
        if b.has_field_by_name("file") {
            let data = b
                .get_field_by_name("file")
                .expect("f")
                .as_bytes()
                .expect("b")
                .to_vec();
            if path == largest {
                let mid = data.len() / 2;
                let mut d = data[..mid].to_vec();
                d.extend((0..100u8).map(|i| i.wrapping_mul(37)));
                d.extend_from_slice(&data[mid..]);
                log.push(format!(
                    "inserted 100 B in the middle of {path} ({} B)",
                    data.len()
                ));
                b.set_field_by_name("file", Value::Bytes(d.into()));
                changed = true;
            } else if path == small {
                let mut d = data;
                d.extend_from_slice(b"# v2\n");
                log.push(format!("appended a line to {path}"));
                b.set_field_by_name("file", Value::Bytes(d.into()));
                changed = true;
            }
        } else if b.has_field_by_name("filesystem") {
            if let Some(Value::Message(sub)) = b.get_field_by_name_mut("filesystem") {
                edit(sub, &path, largest, small, log);
            }
        }
        if changed {
            if let Some(Value::List(x)) = b.get_field_by_name_mut("xattrs") {
                for kv in x.iter_mut() {
                    let Value::Message(kv) = kv else { continue };
                    if kv
                        .get_field_by_name("key")
                        .and_then(|k| k.as_str().map(|s| s == "mtime_ns"))
                        .unwrap_or(false)
                    {
                        kv.set_field_by_name(
                            "value",
                            Value::Bytes(b"1763000000000000000".to_vec().into()),
                        );
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------------------
// stats

fn stats(a: &str, b: &str) {
    let a = fs::read(a).expect("a");
    let b = fs::read(b).expect("b");
    let ha: HashSet<_> = sese::blocks::block_hashes(&a, sese::BLOCK_SIZE)
        .into_iter()
        .collect();
    let hb = sese::blocks::block_hashes(&b, sese::BLOCK_SIZE);
    let mut missing = BTreeSet::new();
    let mut bytes = 0u64;
    for (k, h) in hb.iter().enumerate() {
        if !ha.contains(h) {
            missing.insert(k);
            let (s, e) =
                sese::blocks::block_range(b.len() as u64, sese::BLOCK_SIZE as u64, k as u64)
                    .expect("range");
            bytes += e - s;
        }
    }
    println!(
        "b has {} blocks, {} not in a: {} B of {} B to send",
        hb.len(),
        missing.len(),
        bytes,
        b.len()
    );
}

fn bee_blocks(a: &str, b: &str) {
    let (_, ba) = parse_bee(a);
    let (pb, bb) = parse_bee(b);
    let ha: HashSet<_> = ba.iter().map(|(_, id)| id.clone()).collect();
    let svc = pb.iter().find(|p| p.index == 2).expect("service");
    let mut held_bytes = 0u64;
    let mut held = 0;
    for_each_file(&svc.data, |_, d| {
        if d.len() >= 32 * 1024 && ha.contains(&sha3(d)) {
            held += 1;
            held_bytes += d.len() as u64;
        }
    });
    println!(
        "b has {} blocks, {} in a; held file blocks {held} = {held_bytes} B of {} B",
        bb.len(),
        bb.iter().filter(|(_, id)| ha.contains(id)).count(),
        svc.data.len()
    );
}
