#!/usr/bin/env bash
# Usage: bench.sh <name> <port> <cgroup-dir-resolver-cmd...>
# Measures an already-launched target: waits for HTTP, settles, samples cgroup v2 stats.
set -euo pipefail
name=$1 port=$2 cg=$3 t0=$4 settle=${SETTLE:-60} win=${WIN:-30}

until [ "$(curl -s -o /dev/null -w '%{http_code}' "localhost:$port/" || true)" != "000" ]; do
  [ -d "$cg" ] || { echo "$name: cgroup gone (crashed/OOM?)"; exit 1; }
  sleep 0.2
done
ready=$(( $(date +%s%N) / 1000000 - t0 ))
sleep "$settle"
u0=$(awk '/^usage_usec/{print $2}' "$cg/cpu.stat"); sleep "$win"
u1=$(awk '/^usage_usec/{print $2}' "$cg/cpu.stat")
anon=$(awk '/^anon /{print $2}' "$cg/memory.stat"); file=$(awk '/^file /{print $2}' "$cg/memory.stat")
peak=$(cat "$cg/memory.peak" 2>/dev/null || echo 0)
oom=$(awk '/^oom_kill /{print $2}' "$cg/memory.events")
awk -v n="$name" -v r="$ready" -v a="$anon" -v f="$file" -v p="$peak" -v c="$((u1-u0))" -v w="$win" -v o="$oom" \
  'BEGIN{m=1048576; printf "%s\tready_ms=%d\tanon_MiB=%.1f\tfile_MiB=%.1f\tpeak_MiB=%.1f\tidle_cpu_pct=%.2f\toom_kills=%s\n", n,r,a/m,f/m,p/m,c/(w*10000),o}'
