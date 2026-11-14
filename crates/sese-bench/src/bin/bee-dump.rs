//! Print the records of a .bee file, one per line (first N and markers).
use std::fs::File;
use std::io::BufReader;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let max: usize = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(40);
    let mut pos = 0u64; // bytes of message content so far
    for (i, b) in
        bee_rpc::bee_file::read_bee(BufReader::new(File::open(&a[1]).unwrap())).enumerate()
    {
        let b = b.unwrap();
        let c = b.chunk.as_ref().map_or(0, |c| c.len());
        if i < max {
            let blk = b
                .block
                .as_ref()
                .map(|k| {
                    format!(
                        "block {} plp {:?}",
                        hex::encode(&k.hashes[0].value[..4]),
                        k.previous_lengths_position
                    )
                })
                .unwrap_or_default();
            println!(
                "{i:5} pos {pos:10} head {:?} chunk {c:8} sep {:?} {blk}",
                b.head.as_ref().map(|h| h.index),
                b.separator
            );
        }
        pos += c as u64;
    }
}
