#!/bin/bash
# SeSe over QUIC vs Bee-RPC over gRPC on a real Celaut package, between two network
# namespaces joined by a veth pair. Run as root on Linux. See BENCHMARK.md.
#
#   bench/run.sh <bin dir> <pkg dir> <out dir> [reps] [profiles...]
#
# <pkg dir> holds v1.celaut.bee, v2.celaut.bee, v1.sese, v2.sese and the bee-rpc block
# dir of v1 (v1.celaut.bee.store/blocks), made with `pkgtool` (see BENCHMARK.md).
set -u
BIN=$1; PKG=$2; OUT=$3; REPS=${4:-3}; shift 4 || shift $#
PROFILES=${*:-"lan wan lossy"}
mkdir -p "$OUT"
CSV=$OUT/results.csv
[ -f "$CSV" ] || echo "case,profile,rep,wire_bytes,wire_s2r,wire_r2s,send_wall_s,send_app_s,recv_app_s,send_anon_kb,send_file_kb,recv_anon_kb,recv_file_kb,ok,extra" > "$CSV"

setup() {
  ip netns del sa 2>/dev/null; ip netns del sb 2>/dev/null
  ip netns add sa; ip netns add sb
  ip link add va type veth peer name vb
  ip link set va netns sa; ip link set vb netns sb
  ip -n sa addr add 10.77.0.2/24 dev va; ip -n sb addr add 10.77.0.1/24 dev vb
  for n in sa sb; do ip -n $n link set lo up; done
  ip -n sa link set va up; ip -n sb link set vb up
  # No segmentation offload: netem then sees (and drops) real packets.
  ip netns exec sa ethtool -K va tso off gso off gro off >/dev/null 2>&1
  ip netns exec sb ethtool -K vb tso off gso off gro off >/dev/null 2>&1
  # Large UDP socket buffers for QUIC (both namespaces share the sysctl of the host).
  sysctl -qw net.core.rmem_max=33554432 net.core.wmem_max=33554432
}

profile() {
  for x in "sa va" "sb vb"; do set -- $x
    ip netns exec $1 tc qdisc del dev $2 root 2>/dev/null
    case $P in
      lan) ;;
      wan) ip netns exec $1 tc qdisc add dev $2 root netem delay 20ms rate 100mbit limit 5000 ;;
      lossy) ip netns exec $1 tc qdisc add dev $2 root netem delay 20ms loss 1% rate 100mbit limit 5000 ;;
    esac
  done
}

txb() { ip netns exec $1 cat /sys/class/net/$2/statistics/tx_bytes; }
js() { python3 -c "import json,sys; d=json.loads(open(sys.argv[1]).read().strip().splitlines()[-1]); print(d.get(sys.argv[2], ''))" "$1" "$2" 2>/dev/null; }

run() { # name recv-cmd send-cmd [env]
  local name=$1 rcmd=$2 scmd=$3 envs=${4:-}
  # ONLY="case1 case2": run only these cases.
  if [ -n "${ONLY:-}" ] && ! [[ " $ONLY " == *" $name "* ]]; then return; fi
  local id=$name.$P.$R
  rm -f "$OUT"/recv.out "$OUT"/recv.out.partial "$OUT"/cert.der
  sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null
  # Warm the page cache with the input of the sender and the store of the receiver
  # (for Bee-RPC: the block files of v1, as a node that holds them would have).
  cat "$PKG"/*.sese "$PKG"/*.celaut.bee > /dev/null
  find "$PKG/v1.celaut.bee.store" -type f -exec cat {} + > /dev/null
  local a0=$(txb sa va) b0=$(txb sb vb)
  ip netns exec sb env $envs $rcmd > "$OUT/$id.recv.json" 2> "$OUT/$id.recv.err" &
  local rp=$!
  sleep 1.5
  local t0=$(date +%s.%N)
  ip netns exec sa env $envs timeout 900 $scmd > "$OUT/$id.send.json" 2> "$OUT/$id.send.err"
  local t1=$(date +%s.%N)
  wait $rp
  local a1=$(txb sa va) b1=$(txb sb vb)
  local s2r=$((a1 - a0)) r2s=$((b1 - b0))
  local ok=$( [ "$(js $OUT/$id.send.json ok)" = "True" ] && [ "$(js $OUT/$id.recv.json ok)" = "True" ] && echo 1 || echo 0)
  local extra=""
  case $name in
    sese*) extra="mode=$(js $OUT/$id.send.json mode);wanted=$(js $OUT/$id.send.json blocks_wanted)/$(js $OUT/$id.send.json blocks_total);reused=$(js $OUT/$id.recv.json blocks_reused)";;
    bee*) extra="sha3=$(js $OUT/$id.recv.json service_sha3 | cut -c1-12);blocks_seen=$(js $OUT/$id.recv.json blocks_seen)";;
  esac
  echo "$name,$P,$R,$((s2r + r2s)),$s2r,$r2s,$(python3 -c "print(round($t1-$t0,3))"),$(js $OUT/$id.send.json elapsed_s),$(js $OUT/$id.recv.json elapsed_s),$(js $OUT/$id.send.json peak_anon_kb),$(js $OUT/$id.send.json peak_file_kb),$(js $OUT/$id.recv.json peak_anon_kb),$(js $OUT/$id.recv.json peak_file_kb),$ok,$extra" | tee -a "$CSV"
}

tcpcc() { for n in sa sb; do ip netns exec $n sysctl -qw net.ipv4.tcp_congestion_control=$1; done; }

SR="$BIN/sese-recv 10.77.0.1:4433 $OUT/cert.der $OUT/recv.out"
SS="$BIN/sese-send 10.77.0.1:4433 $OUT/cert.der"
BR="$BIN/bee-recv 10.77.0.1:50051 $OUT/recv.out"
BS="$BIN/bee-send http://10.77.0.1:50051"
V1BLOCKS=$PKG/v1.celaut.bee.store/blocks

modprobe tcp_bbr 2>/dev/null
setup
for P in $PROFILES; do
  profile
  reps=$REPS; [ "$P" = lossy ] && reps=$(( REPS < 2 ? REPS : 2 ))
  for R in $(seq 1 $reps); do
    tcpcc cubic
    # Cold: the receiver holds nothing.
    run sese-zstd   "$SR" "$SS $PKG/v1.sese"
    [ "$P" != lossy ] && run sese-raw "$SR" "$SS $PKG/v1.sese raw"
    run bee-grpc    "$BR" "$BS $PKG/v1.celaut.bee"
    # Dedup: the receiver holds v1, the sender sends v2.
    run sese-dedup  "$SR $PKG/v1.sese" "$SS $PKG/v2.sese"
    run bee-dedup   "$BR $V1BLOCKS" "$BS $PKG/v2.celaut.bee"
    [ "$P" != lossy ] && run bee-dedup-defwin "$BR $V1BLOCKS" "$BS $PKG/v2.celaut.bee" BEE_WINDOW=default
    if [ "$P" = lossy ]; then
      # Same congestion control on both: BBR for QUIC (quinn) and for TCP (kernel).
      run sese-zstd-bbr  "$SR" "$SS $PKG/v1.sese" SESE_BBR=1
      run sese-dedup-bbr "$SR $PKG/v1.sese" "$SS $PKG/v2.sese" SESE_BBR=1
      tcpcc bbr
      run bee-grpc-bbr   "$BR" "$BS $PKG/v1.celaut.bee"
      run bee-dedup-bbr  "$BR $V1BLOCKS" "$BS $PKG/v2.celaut.bee"
      tcpcc cubic
    fi
  done
done
ip netns del sa; ip netns del sb
echo DONE
