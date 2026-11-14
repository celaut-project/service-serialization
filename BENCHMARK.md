# Benchmark: SeSe over QUIC vs Bee-RPC over gRPC, on a real Celaut service

**Question (Josemi):** could SeSe transport Celaut services the way Bee-RPC does today?

**Short answer:** for moving a service package, yes, and better: on the same package it puts
**3× fewer bytes on the wire** for a first transfer (zstd), **~90× fewer** for a new version
of a service that the receiver already has (block negotiation before any data), and the
receiver needs **~4 MB of memory instead of ~150 MB**. It is not a replacement for Bee-RPC
as an RPC framework (see "What is missing" at the end).

## The package

- **v1** = the real echo-chat service `b4e0438b85219a34bc6a6da70adcb786f04ba408eabfaac141e7e6c71c1a2dfc`
  (`nodo export` from test node n538c, 138 488 746 bytes as `.celaut.bee`): 4 917 files
  (137 MB), 632 directories, 604 links. Bee-RPC frames it as 2 785 records, 1 block for the
  filesystem and 684 blocks for the files of at least 32 KiB.
- **v2** = v1 after a small change, as a rebuild of the service would make: a line appended
  to one small file early in the tree (everything after it moves), 100 bytes inserted in the
  middle of the largest file (`/usr/bin/python3.11`, 6.8 MB), and a new 300 KiB file. New
  Celaut id `354779ca…`.
- **Bee-RPC form:** `pkgtool mkbee` writes the package as the node's store does (a
  multiblock directory) and reads it back with `bee_rpc::store::Store` (block depth 2), as an
  export does. Check: on v1 it gives the real export **byte for byte**, so v2's `.celaut.bee`
  is what the node would send for v2.
- **SeSe form:** `pkgtool tosese`: v1 = 147 471 949 bytes (+6.6 % over the proto, the
  alignment padding; see the choice of block size below), 9 001 blocks of 16 KiB. The way back
  gives the same proto bytes, so the **Celaut id `b4e0438b…` is kept**. Encode 0.8 s, decode
  to proto 0.5 s, zero-copy walk of all 4 917 files 1.5 ms.

## Method

- Test machine: AMD Ryzen 5 5600X (6 cores, 12 threads), 23 GB RAM for WSL2 Ubuntu 22.04,
  kernel 6.6; release builds, Rust 1.97.
- Sender and receiver are separate processes in two **network namespaces joined by a veth
  pair**, segmentation offloads off so that netem sees real packets.
- **Bytes on the wire** = the `tx_bytes` of both veth ends (both directions, including
  Ethernet/IP/UDP/TCP/QUIC/TLS/HTTP/2 headers and retransmissions).
- **Time** = from the start of the transfer (QUIC: the stream opens; gRPC: the call starts)
  to the sender's receipt of the receiver's answer that the file is complete and verified.
  Connection setup and process exit are not in it (QUIC's close drain adds ~0.2–7 s at
  process exit, which is not transfer time).
- **Memory** = peak of `RssAnon` (heap: what the process owns) and of `RssFile` (pages of
  mapped files = page cache, shared) sampled every 10 ms in each process. The SeSe sender
  maps the file it sends (zero-copy), so its file pages are the package itself.
- Page cache dropped before each run, then the inputs and the receiver's held data warmed
  (for Bee-RPC: v1's block files). The first full run did not warm Bee-RPC's block files, and
  its dedup rows read them from a cold disk (6.5 s instead of 1.2 s on lan); those three cases
  were run again with the fix and `results.csv` has the second run.
- Links (`tc netem` on both veth ends): **lan** = no shaping (CPU-bound); **wan** = 100 Mbit/s,
  20 ms each way (40 ms RTT); **lossy** = wan + 1 % loss each way. lan and wan 3 runs, lossy 2
  runs; the tables give the median. All runs: both sides OK and the received file has the
  expected id (SeSe: BLAKE3; Bee-RPC: the receiver's SHA3-256 of the Service = the Celaut id).
- **SeSe/QUIC:** `sese-send` / `sese-recv` (quinn 0.11, TLS 1.3, CUBIC unless BBR is named;
  16 MiB windows). The receiver holds v1 as a file (`FileStore`: a hash → offset index of
  `v1.sese`, built before the connection; a node would keep it).
- **Bee-RPC/gRPC:** `bee-send` / `bee-recv` on **bee-rpc-rs v0.4.2** (the Rust port that
  chatui uses; same wire format as the node's Python bee-rpc) over tonic 0.14, plaintext
  HTTP/2 (as nodo), TCP_NODELAY. The sender streams the `.celaut.bee` records with
  `grpc::call_buffers` with `block_skip` (so the receiver can skip held blocks). The receiver
  parses with `frame::Parser` (each block checked against its SHA3 id) with a block store of
  v1's block files (as the node's `__block__`) and a `StreamControl` that asks the sender to
  skip the blocks it holds. HTTP/2 windows: 16 MiB stream / 32 MiB connection (the fastest
  setting for Bee-RPC here; hyper's adaptive window was erratic, 2.6–30 s on the loopback;
  hyper's defaults are the extra `default HTTP/2 windows` row).
- Reproduce: `bench/run.sh <bin dir> <pkg dir> <out dir> 3 lan wan lossy`; raw rows in
  `bench/results.csv`; tables by `bench/report.py bench/results.csv`.

## Results

### unshaped veth (CPU-bound) (`lan`)

| Case | n | Wire MB (both ways) | Time s | Sender peak MB (anon / file) | Receiver peak MB (anon / file) | All OK |
|---|---|---|---|---|---|---|
| SeSe/QUIC, cold (zstd stream) | 3 | 48.06 | 0.66 | 6 / 145 | 5 / 5 | yes |
| SeSe/QUIC, cold, no zstd | 3 | 153.76 | 0.73 | 12 / 145 | 34 / 5 | yes |
| Bee-RPC/gRPC, cold | 3 | 146.25 | 1.10 | 18 / 4 | 157 / 4 | yes |
| SeSe/QUIC, v2 with v1 held | 3 | 1.57 | 0.27 | 1 / 146 | 4 / 5 | yes |
| Bee-RPC/gRPC, v2 with v1 held | 3 | 142.46 | 1.23 | 18 / 4 | 199 / 4 | yes |
| Bee-RPC/gRPC, v2 with v1 held, default HTTP/2 windows | 3 | 139.82 | 2.73 | 19 / 4 | 151 / 4 | yes |

### 100 Mbit/s, 40 ms RTT (`wan`)

| Case | n | Wire MB (both ways) | Time s | Sender peak MB (anon / file) | Receiver peak MB (anon / file) | All OK |
|---|---|---|---|---|---|---|
| SeSe/QUIC, cold (zstd stream) | 3 | 48.22 | 4.31 | 24 / 145 | 4 / 5 | yes |
| SeSe/QUIC, cold, no zstd | 3 | 152.82 | 12.82 | 21 / 145 | 2 / 5 | yes |
| Bee-RPC/gRPC, cold | 3 | 146.42 | 12.28 | 21 / 4 | 140 / 4 | yes |
| SeSe/QUIC, v2 with v1 held | 3 | 1.56 | 0.58 | 2 / 146 | 4 / 5 | yes |
| Bee-RPC/gRPC, v2 with v1 held | 3 | 142.78 | 11.98 | 22 / 4 | 150 / 4 | yes |
| Bee-RPC/gRPC, v2 with v1 held, default HTTP/2 windows | 3 | 139.91 | 15.96 | 18 / 4 | 151 / 4 | yes |

### 100 Mbit/s, 40 ms RTT, 1 % loss each way (`lossy`)

| Case | n | Wire MB (both ways) | Time s | Sender peak MB (anon / file) | Receiver peak MB (anon / file) | All OK |
|---|---|---|---|---|---|---|
| SeSe/QUIC, cold (zstd stream) | 2 | 48.38 | 51.49 | 21 / 145 | 4 / 5 | yes |
| SeSe/QUIC, cold (zstd), BBR | 2 | 48.43 | 4.33 | 22 / 145 | 22 / 5 | yes |
| Bee-RPC/gRPC, cold | 2 | 148.00 | 254.93 | 20 / 4 | 140 / 4 | yes |
| Bee-RPC/gRPC, cold, TCP BBR | 2 | 146.05 | 12.58 | 19 / 4 | 146 / 4 | yes |
| SeSe/QUIC, v2 with v1 held | 2 | 1.57 | 0.76 | 2 / 146 | 3 / 5 | yes |
| SeSe/QUIC, v2 with v1 held, BBR | 2 | 1.57 | 0.49 | 2 / 146 | 4 / 5 | yes |
| Bee-RPC/gRPC, v2 with v1 held | 2 | 144.32 | 245.22 | 20 / 4 | 150 / 4 | yes |
| Bee-RPC/gRPC, v2 with v1 held, TCP BBR | 2 | 142.45 | 12.19 | 19 / 4 | 155 / 4 | yes |

## What the numbers say

1. **First transfer (the receiver has nothing).** SeSe sends one zstd stream: **48 MB on the
   wire against 146 MB for Bee-RPC (−67 %)**, so on a 100 Mbit/s link **4.3 s against 12.3 s**.
   Without zstd SeSe is a little larger than Bee-RPC (153 MB: the file has +6.6 % padding),
   so the gain is the compression, which the README asks for "when there are no blocks".
2. **New version (the receiver holds v1).** SeSe's receiver answers the header with a bitmap
   of the 232 blocks (3.8 MB raw) it lacks out of 9 021; with zstd per block that is **1.6 MB on
   the wire, including the 288 KB header**. Bee-RPC sends **~142 MB, almost the whole package**:
   its skip is reactive. The receiver can ask to skip a block only after its start marker
   arrives, and by then the block (most files are one chunk of < 1 MiB) is already committed to
   the HTTP/2 stream. Of the 107 MB of held file blocks, it saved 3.5–6 MB whatever the
   window size. Also, the filesystem itself is one block whose id changes with any file, so
   its ~32 MB that are not file blocks (files under 32 KiB and the tree) are always resent. **On 100 Mbit/s: 0.6 s against 12 s.**
3. **Lossy link.** With 1 % loss both CUBIC stacks collapse (QUIC 51 s for 48 MB; TCP 255 s for
   146 MB), and both recover with BBR (QUIC 4.3 s; TCP 12.6 s). The gain is again the bytes,
   not QUIC itself: QUIC's own advantages (no head-of-line blocking across streams, 0/1-RTT
   setup, connection migration) do not matter for one large stream.
4. **Memory.** The SeSe receiver writes verified blocks straight to the file: **~4 MB peak**
   (QUIC buffers). The Bee-RPC receiver (`frame::Parser`) builds the whole Service message in
   memory: **~140–200 MB**, the size of the package. (The node's Python receiver in disk mode
   writes blocks to disk; this is the Rust crate.) The SeSe sender's heap is ~6–24 MB; its file
   pages (~145 MB) are the mapped package in the page cache, shared and reclaimable.
5. **CPU (lan).** Cold: SeSe 0.66 s with zstd level 3 (one core) vs Bee-RPC 1.10 s; v2: SeSe
   0.27 s vs Bee-RPC 1.2 s.

## Choice of block size and alignment (`explore`)

On the same v1/v2 (proto v1 = 138 323 666 B; its zstd-3 = 44.9 MB):

| block | align from | SeSe size | over proto | zstd-3 | blocks | v2 needs |
|---|---|---|---|---|---|---|
| 4 KiB | 32 KiB | 144 347 049 | +4.35 % | 45 720 288 | 35 241 | 3 653 735 |
| 8 KiB | 32 KiB | 147 922 857 | +6.94 % | 45 730 668 | 18 057 | 3 715 175 |
| 16 KiB | 32 KiB | 155 049 897 | +12.09 % | 45 793 952 | 9 464 | 3 788 903 |
| **16 KiB** | **64 KiB** | **147 471 949** | **+6.61 %** | **45 727 081** | **9 001** | **3 788 903** |
| 64 KiB | 64 KiB | 167 525 965 | +21.11 % | 45 983 309 | 2 557 | 4 198 503 |
| 16 KiB | off | 141 421 325 | +2.24 % | 45 691 489 | 8 632 | 141 532 659 |

Without alignment, fixed-size blocks dedup nothing after the first change (v2 needs 99.9 %).
16 KiB blocks aligned from 64 KiB: small header (32 B × 9 001 = 288 KB), +6.6 % on disk (zero
padding, which zstd removes on the wire), and the same dedup as smaller thresholds on this
package, because after each aligned file the layout is on a block boundary again, so a shift
stops there. 4 KiB blocks have less padding but a 1.1 MB header.

## Verdict

**Can SeSe+QUIC replace Bee-RPC for service transport?** For the transport of a sealed
service package (upload to a node with StartService, export/import, node-to-node
distribution): **yes**, and it is better on every measure here: 3× fewer bytes cold, ~90×
fewer for an update, constant memory on the receiver, verification per block and per file,
resume after a cut, and the Celaut id kept (the SeSe file maps back to the same proto bytes).

**What is missing (Bee-RPC does it, this does not):**

- **RPC.** Bee-RPC is the framing of every gRPC call between client, node and service:
  several typed messages per call (`Head.index`), streaming parts of any size, the flow-control
  `signal`, skip requests during a call, and the generated gRPC services (`StartService`,
  `EstimatedCost`, …). SeSe here is a file format plus one transfer protocol; a call needs
  either SeSe messages inside the existing gRPC methods or a new RPC layer over QUIC streams.
- **Blocks inside other messages.** Bee-RPC can make any field of any message a block; SeSe
  dedups at fixed 16 KiB blocks of one file (stronger dedup, but only for the file).
- **Streaming production.** The SeSe writer needs the whole value (absolute offsets, lengths
  before data); Bee-RPC can send a message whose end is not yet produced.
- **Peers that exist.** Nodes speak gRPC + Bee-RPC today; QUIC also needs UDP to pass the
  firewall/NAT (nodo's nftables, Tor: Tor carries TCP only, so a Tor route needs the gRPC path).
- **Identity.** The SeSe id (BLAKE3) and the Celaut id (SHA3 of the proto) differ; the
  mapping keeps the Celaut id computable from the SeSe file, but on-chain references, the
  node registry and the packer use the SHA3 id.

**Migration cost (estimate):**

- Rust side (chatui light client): small; it can use these crates as they are (~1–2 weeks to
  add a QUIC upload next to the gRPC one, plus tests on a real node).
- Node (Python): a SeSe reader/writer (~500 lines; the format is simple) and the QUIC
  receiver (aioquic) or the Rust crate through PyO3 (~2–3 weeks with tests); the block store
  becomes "SeSe files + a hash → offset index" instead of per-file SHA3 blocks; a firewall
  rule for UDP.
- Packer: write the SeSe file next to the `.celaut.bee` (or convert at the node; conversion is
  0.8 s for this package).
- Compatibility: keep gRPC/Bee-RPC as the fallback (Tor, old peers) and announce `sese/1` as a
  capability; nothing on-chain changes if the Celaut id stays the SHA3 of the proto.
