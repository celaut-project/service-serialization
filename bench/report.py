#!/usr/bin/env python3
"""Median of each case and profile from bench/run.sh's results.csv, as Markdown."""
import csv, statistics, sys
from collections import defaultdict

rows = list(csv.DictReader(open(sys.argv[1])))
groups = defaultdict(list)
for r in rows:
    groups[(r["profile"], r["case"])].append(r)

def med(rs, k):
    vals = [float(r[k]) for r in rs if r[k] not in ("", None)]
    return statistics.median(vals) if vals else float("nan")

order = ["sese-zstd", "sese-zstd-bbr", "sese-raw", "bee-grpc", "bee-grpc-bbr",
         "sese-dedup", "sese-dedup-bbr", "bee-dedup", "bee-dedup-bbr", "bee-dedup-defwin"]
label = {
    "sese-zstd": "SeSe/QUIC, cold (zstd stream)",
    "sese-zstd-bbr": "SeSe/QUIC, cold (zstd), BBR",
    "sese-raw": "SeSe/QUIC, cold, no zstd",
    "bee-grpc": "Bee-RPC/gRPC, cold",
    "bee-grpc-bbr": "Bee-RPC/gRPC, cold, TCP BBR",
    "sese-dedup": "SeSe/QUIC, v2 with v1 held",
    "sese-dedup-bbr": "SeSe/QUIC, v2 with v1 held, BBR",
    "bee-dedup": "Bee-RPC/gRPC, v2 with v1 held",
    "bee-dedup-bbr": "Bee-RPC/gRPC, v2 with v1 held, TCP BBR",
    "bee-dedup-defwin": "Bee-RPC/gRPC, v2 with v1 held, default HTTP/2 windows",
}
profiles = {"lan": "unshaped veth (CPU-bound)", "wan": "100 Mbit/s, 40 ms RTT", "lossy": "100 Mbit/s, 40 ms RTT, 1 % loss each way"}
for p, title in profiles.items():
    present = [c for c in order if (p, c) in groups]
    if not present:
        continue
    print(f"\n### {title} (`{p}`)\n")
    print("| Case | n | Wire MB (both ways) | Time s | Sender peak MB (anon / file) | Receiver peak MB (anon / file) | All OK |")
    print("|---|---|---|---|---|---|---|")
    for c in present:
        rs = groups[(p, c)]
        ok = all(r["ok"] == "1" for r in rs)
        print(f"| {label[c]} | {len(rs)} | {med(rs,'wire_bytes')/1e6:.2f} | {med(rs,'send_app_s'):.2f} | "
              f"{med(rs,'send_anon_kb')/1024:.0f} / {med(rs,'send_file_kb')/1024:.0f} | "
              f"{med(rs,'recv_anon_kb')/1024:.0f} / {med(rs,'recv_file_kb')/1024:.0f} | {'yes' if ok else 'NO'} |")
