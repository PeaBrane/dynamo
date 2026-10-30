#!/usr/bin/env bash
# SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
# This wrapper runs on the measurement host. Setup/event lanes are idle during queries.
# Optional runtime capsule is process-local; it never modifies the host runtime.
binary=$1
mode=$2
output=$3
case_name=${4:-all}
window_ms=${5:-250}
bench_cpu=${BENCH_CPU:-2}
read_cpu() { awk -v cpu="cpu${bench_cpu}" '$1 == cpu { print $2+$3+$4+$7+$8, $9 }' /proc/stat; }
read -r busy_before steal_before < <(read_cpu)
start_ns=$(date +%s%N)
launcher=()
if [[ -n ${BENCH_LOADER:-} ]]; then
  launcher=("$BENCH_LOADER" --library-path "${BENCH_LIBDIR:?set BENCH_LIBDIR}")
fi
/usr/bin/time -f '%U %S' -o "${output}.cpu" \
  taskset -c "$bench_cpu" "${launcher[@]}" "$binary" "$mode" "$output" "$case_name" "$window_ms"
end_ns=$(date +%s%N)
read -r busy_after steal_after < <(read_cpu)
read -r user_seconds system_seconds < "${output}.cpu"
awk -v before="$busy_before" -v after="$busy_after" -v sb="$steal_before" -v sa="$steal_after" \
  -v hz="$(getconf CLK_TCK)" -v u="$user_seconds" -v s="$system_seconds" \
  -v start="$start_ns" -v end="$end_ns" -v cpu="$bench_cpu" \
  'BEGIN { printf "{\"cpu\":%d,\"busy_seconds\":%.6f,\"process_cpu_seconds\":%.6f,\"foreign_cpu_seconds\":%.6f,\"steal_seconds\":%.6f,\"wall_seconds\":%.6f}\n", cpu,(after-before)/hz,u+s,(after-before)/hz-u-s,(sa-sb)/hz,(end-start)/1e9 }' \
  > "${output}.host.json"
