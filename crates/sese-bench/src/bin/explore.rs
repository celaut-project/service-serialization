//! Choose the block size and the alignment threshold on a real package:
//! `explore <v1/service.pb> <v2/service.pb>`. For each option: SeSe size, zstd size,
//! number of blocks, and the bytes that v2 needs when the receiver holds v1.
use sese::WriterOptions;
use std::collections::HashSet;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let v1 = std::fs::read(&a[1]).unwrap();
    let v2 = std::fs::read(&a[2]).unwrap();
    let d = sese_celaut::descriptor(sese_celaut::SERVICE).unwrap();
    println!(
        "proto v1 {} B, zstd-3 {} B",
        v1.len(),
        zstd::bulk::compress(&v1, 3).unwrap().len()
    );
    println!(
        "{:>6} {:>8} | {:>11} {:>7} | {:>11} | {:>7} | {:>10} {:>6}",
        "block", "align", "sese B", "over%", "zstd-3 B", "blocks", "v2 need B", "need%"
    );
    for (b, al) in [
        (4096u64, 8192u64),
        (4096, 16384),
        (4096, 32768),
        (8192, 16384),
        (8192, 32768),
        (16384, 16384),
        (16384, 32768),
        (16384, 65536),
        (65536, 65536),
        (16384, u64::MAX),
    ] {
        let o = WriterOptions {
            block_size: b,
            align_min: al,
        };
        let s1 = sese_celaut::proto_to_sese_with(&d, &v1, o).unwrap();
        let s2 = sese_celaut::proto_to_sese_with(&d, &v2, o).unwrap();
        let z = zstd::bulk::compress(&s1, 3).unwrap().len();
        let h1: HashSet<_> = sese::blocks::block_hashes(&s1, b as usize)
            .into_iter()
            .collect();
        let mut need = 0u64;
        for (k, h) in sese::blocks::block_hashes(&s2, b as usize)
            .iter()
            .enumerate()
        {
            if !h1.contains(h) {
                let (x, y) = sese::blocks::block_range(s2.len() as u64, b, k as u64).unwrap();
                need += y - x;
            }
        }
        println!(
            "{:>6} {:>8} | {:>11} {:>6.2}% | {:>11} | {:>7} | {:>10} {:>5.2}%",
            b,
            if al == u64::MAX {
                "off".into()
            } else {
                al.to_string()
            },
            s1.len(),
            100.0 * (s1.len() as f64 / v1.len() as f64 - 1.0),
            z,
            sese::blocks::block_count(s1.len() as u64, b),
            need,
            100.0 * need as f64 / s2.len() as f64
        );
    }
}
