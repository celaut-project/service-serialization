//! Look inside a `.celaut.bee` package: its records, its parts, its blocks, and the
//! files of the service. Usage: `bee-inspect <file.celaut.bee>`.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;

use bee_rpc::bee_file::read_bee;
use bee_rpc::frame::{ParseOptions, Parser};
use prost_reflect::{DynamicMessage, ReflectMessage, Value};
use sha3::{Digest, Sha3_256};

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: bee-inspect <file.celaut.bee>");
    let mut records = 0u64;
    let mut record_bytes = 0u64;
    let mut chunk_bytes = 0u64;
    let mut markers = 0u64;
    let mut heads = BTreeMap::new();
    let mut parser = Parser::new(ParseOptions::default());
    let mut parts = Vec::new();
    for buffer in read_bee(BufReader::new(File::open(&path).expect("open"))) {
        let buffer = buffer.expect("record");
        records += 1;
        record_bytes += prost::Message::encoded_len(&buffer) as u64 + 4;
        chunk_bytes += buffer.chunk.as_ref().map_or(0, |c| c.len() as u64);
        if buffer.block.is_some() {
            markers += 1;
        }
        if let Some(h) = &buffer.head {
            *heads.entry(h.index).or_insert(0u64) += 1;
        }
        parts.extend(parser.push(buffer).expect("parse"));
    }
    parts.extend(parser.finish().expect("finish"));
    println!("records {records}, record bytes {record_bytes}, chunk bytes {chunk_bytes}, block markers {markers}");
    println!("heads {heads:?}");
    let starts = parser.block_starts();
    let mut depths = BTreeMap::new();
    for s in starts {
        *depths.entry(s.depth).or_insert(0u64) += 1;
    }
    println!("blocks {} by depth {depths:?}", starts.len());
    let pool = sese_celaut::pool();
    let service_desc = pool.get_message_by_name("celaut.Service").expect("Service");
    let fs_desc = pool
        .get_message_by_name("celaut.Service.Container.Filesystem")
        .expect("Filesystem");
    for part in &parts {
        let sha = hex::encode(Sha3_256::digest(&part.data));
        println!(
            "part index {} len {} sha3 {}",
            part.index,
            part.data.len(),
            sha
        );
        if let Ok(service) = DynamicMessage::decode(service_desc.clone(), part.data.as_slice()) {
            let reencoded = prost::Message::encode_to_vec(&service);
            println!(
                "  Service decodes; re-encode identical: {}",
                reencoded == part.data
            );
            let container = service.get_field_by_name("container").expect("container");
            let container = container.as_message().expect("msg");
            let fs = container.get_field_by_name("filesystem").expect("fs");
            let fs = fs.as_bytes().expect("bytes");
            println!("  filesystem bytes {}", fs.len());
            match DynamicMessage::decode(fs_desc.clone(), fs.as_ref()) {
                Ok(tree) => {
                    let re = prost::Message::encode_to_vec(&tree);
                    println!(
                        "  Filesystem decodes; re-encode identical: {}",
                        re.as_slice() == fs.as_ref()
                    );
                    let mut stats = Stats::default();
                    walk(&tree, 0, &mut stats);
                    stats.print();
                }
                Err(e) => println!("  filesystem is not a Filesystem: {e}"),
            }
            let mut copy = service.clone();
            if let Some(Value::Message(c)) = copy.get_field_by_name_mut("container") {
                c.clear_field_by_name("filesystem");
            }
            println!(
                "  service without filesystem: {} bytes",
                prost::Message::encoded_len(&copy)
            );
            let _ = copy.descriptor();
        }
    }
}

#[derive(Default)]
struct Stats {
    files: u64,
    dirs: u64,
    links: u64,
    file_bytes: u64,
    max_depth: usize,
    /// Count and bytes of files by power-of-two size class.
    classes: BTreeMap<u32, (u64, u64)>,
    xattr_keys: BTreeMap<String, u64>,
}

impl Stats {
    fn print(&self) {
        println!(
            "  files {} ({} bytes), dirs {}, links {}, max depth {}",
            self.files, self.file_bytes, self.dirs, self.links, self.max_depth
        );
        for (class, (n, b)) in &self.classes {
            println!(
                "    < 2^{class:>2} ({:>10} B): {n:>6} files {b:>12} bytes",
                1u64 << class
            );
        }
        println!("  xattr keys {:?}", self.xattr_keys);
    }
}

fn walk(tree: &DynamicMessage, depth: usize, stats: &mut Stats) {
    stats.max_depth = stats.max_depth.max(depth);
    let branches = tree.get_field_by_name("branch").expect("branch");
    for b in branches.as_list().expect("list") {
        let b = b.as_message().expect("ItemBranch");
        for x in b
            .get_field_by_name("xattrs")
            .expect("x")
            .as_list()
            .expect("l")
        {
            let k = x
                .as_message()
                .expect("kv")
                .get_field_by_name("key")
                .expect("key");
            let k = String::from_utf8_lossy(
                k.as_str()
                    .map(|s| s.as_bytes())
                    .unwrap_or_else(|| k.as_bytes().map(|b| b.as_ref()).unwrap_or(b"")),
            )
            .to_string();
            *stats.xattr_keys.entry(k).or_insert(0) += 1;
        }
        if b.has_field_by_name("file") {
            let f = b.get_field_by_name("file").expect("file");
            let len = f.as_bytes().expect("bytes").len() as u64;
            stats.files += 1;
            stats.file_bytes += len;
            let class = 64 - len.leading_zeros();
            let e = stats.classes.entry(class).or_insert((0, 0));
            e.0 += 1;
            e.1 += len;
        } else if b.has_field_by_name("link") {
            stats.links += 1;
        } else if b.has_field_by_name("filesystem") {
            stats.dirs += 1;
            let sub = b.get_field_by_name("filesystem").expect("fs");
            walk(sub.as_message().expect("Filesystem"), depth + 1, stats);
        }
    }
}
