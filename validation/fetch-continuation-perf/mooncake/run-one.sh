#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
run_root=$1
variant=$2
duration=$3
output=$4
read_cpu() {
  awk '$1 ~ /^cpu[0-9]+$/ { n=substr($1,4)+0; if(n>=2 && n<=15) { busy+=$2+$3+$4+$7+$8; steal+=$9 } } END { print busy,steal }' /proc/stat
}
read -r busy_before steal_before < <(read_cpu)
start_ns=$(date +%s%N)
/usr/bin/time -f '%U %S' -o "$output.cpu" \
  taskset -c 2-15 "$run_root/runtime/ld-linux-x86-64.so.2" --library-path "$run_root/runtime" \
  "$run_root/bins/$variant" "$run_root/traces/agentx128.jsonl" \
  --num-gpu-blocks 131072 --num-host-blocks 32768 --kvcr-blocks 262144 \
  --num-unique-inference-workers 16 --block-size 16 --trace-block-size 64 --seed 42 \
  --query-lanes 2 --issuer-threads 1 --issuer-cpus 14 --query-issuer-cpu 15 --backend-cpus 2-13 \
  --benchmark-duration-ms "$duration" --result-json-output "$output" \
  concurrent-radix-tree-compressed --num-event-workers 4
end_ns=$(date +%s%N)
read -r busy_after steal_after < <(read_cpu)
read -r user_seconds system_seconds < "$output.cpu"
awk -v before="$busy_before" -v after="$busy_after" -v sb="$steal_before" -v sa="$steal_after" \
  -v hz="$(getconf CLK_TCK)" -v u="$user_seconds" -v s="$system_seconds" \
  -v start="$start_ns" -v end="$end_ns" \
  'BEGIN { printf "{\"busy_seconds\":%.6f,\"process_cpu_seconds\":%.6f,\"foreign_cpu_seconds\":%.6f,\"steal_seconds\":%.6f,\"whole_process_wall_seconds\":%.6f}\n",(after-before)/hz,u+s,(after-before)/hz-u-s,(sa-sb)/hz,(end-start)/1e9 }' > "$output.host.json"
