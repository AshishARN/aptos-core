#!/usr/bin/env bash
set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
BINARY="cargo run --release -p aptos-executor-benchmark --"
DATA_DIR="/tmp/dag-bench-db"
CHECKPOINT_DIR="/tmp/dag-bench-checkpoint"
LOG_DIR="/tmp/dag-overhead-logs"
BLOCKS=1
SLEEP_SECS=15

BLOCK_SIZES=(10000)

WORKLOADS=(
  # "non-conflicting-coin-transfer"
  # "liquidity-pool-swap"
  # "modify-global-resource"
  "non-conflicting-coin-transfer"
  "monotonic-counter-multiple1000"
  "monotonic-counter-multiple100"
  "monotonic-counter-multiple10"
  "monotonic-counter-single"
  "apt-fa-transfer"
  "liquidity-pool-swap"
  "liquidity-pool-swap-stable"
  "modify-global-resource"
  "modify-global-resource-agg-v2"
  # "token-v1nft-mint-and-store-sequential"
  # "token-v1nft-mint-and-store-parallel"
  "token-v2-ambassador-mint"
  "fungible-asset-mint"
)

EXECUTOR_TYPE="aptos-vm-with-block-stm"

mkdir -p "$LOG_DIR"

# ── Run ───────────────────────────────────────────────────────────────────────
run_workload() {
  local workload="$1"
  local block_size="$2"
  local log_file="$LOG_DIR/${workload}__bs${block_size}.log"

  echo ""
  echo "════════════════════════════════════════════════════════"
  echo " workload   : $workload"
  echo " block_size : $block_size"
  echo " log        : $log_file"
  echo "════════════════════════════════════════════════════════"

  $BINARY \
    --block-executor-type "$EXECUTOR_TYPE" \
    --block-size "$block_size" \
    run-executor \
    --blocks "$BLOCKS" \
    --transaction-type "$workload" \
    --data-dir "$DATA_DIR" \
    --checkpoint-dir "$CHECKPOINT_DIR" \
    2>&1 | tee "$log_file"
}

# ── Extraction functions ───────────────────────────────────────────────────────
# Each function matches its specific field=VALUE directly on the same line,
# rather than using tail -1 on all PARTITION-TIMING lines (which would always
# return the last line = total_partition, making analysis_ms and dag_ms 0).

extract_analysis_ms() {
  grep -oP '\[PARTITION-TIMING\].*static_analysis=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_dag_ms() {
  grep -oP '\[PARTITION-TIMING\].*dag_construction=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_partition_total_ms() {
  grep -oP '\[PARTITION-TIMING\].*total_partition=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_scope_ms() {
  grep -oP '\[DAG-TIMING\].*scope_wall=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_commit_work_ms() {
  grep -oP '\[COMMITTER\].*work_total=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_commit_wait_ms() {
  grep -oP '\[COMMITTER\].*wait_total=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_mat_tail_ms() {
  grep -oP 'mat_tail \(post-scope\): \K[0-9]+' "$1" | tail -1 || echo "0"
}

# From [DAG-OVERHEAD] line added in executor.rs
extract_overhead_total_ms() {
  grep -oP '\[DAG-OVERHEAD\].*total_ms=\K[0-9]+' "$1" | tail -1 || echo "0"
}

extract_num_txns() {
  grep -oP '\[DAG-OVERHEAD\].*num_txns=\K[0-9]+' "$1" | tail -1 || echo "0"
}

# ── Main ──────────────────────────────────────────────────────────────────────
echo ""
echo "Step 1: Creating DB (skip if already exists)"
if [ ! -d "$DATA_DIR" ]; then
  $BINARY \
    --block-executor-type aptos-vm-with-block-stm \
    create-db \
    --data-dir "$DATA_DIR" \
    --num-accounts 1000000
else
  echo "  DB already exists at $DATA_DIR, skipping."
fi

echo ""
echo "Step 2: Running workloads"
echo ""

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    run_workload "$workload" "$block_size"
    echo "  [Sleeping ${SLEEP_SECS}s to cool down...]"
    sleep "$SLEEP_SECS"
  done
done

# ── Phase Overhead Summary ────────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════════════════════════════════════════════════"
echo " PHASE OVERHEAD SUMMARY"
echo " IMPORTANT — what each column means:"
echo "   ANLYS_ms  = static analysis (par rayon over txns)         — runs BEFORE executor"
echo "   DAG_ms    = DAG edge construction + wave scheduling        — runs BEFORE executor"
echo "   PART_ms   = ANLYS + DAG combined (total partition phase)   — runs BEFORE executor"
echo "   EXEC_ms   = parallel execution wall time (scope_wall)      — the actual worker threads"
echo "   COMMIT_ms = delayed-field commit inside committer thread   — OVERLAPS with EXEC_ms, not additive"
echo "   MAT_ms    = post-scope materialization tail                — runs AFTER scope closes"
echo "   TOTAL_ms  = full execute_transactions_dag wall time"
echo "═══════════════════════════════════════════════════════════════════════════════════════════════════════════"

printf "%-36s  %6s  %8s  %6s  %7s  %7s  %7s  %6s  %8s  |  %6s  %5s  %6s  %6s\n" \
  "WORKLOAD" "BS" "ANLYS_ms" "DAG_ms" "PART_ms" "EXEC_ms" "COMMIT_ms" "MAT_ms" "TOTAL_ms" \
  "ANLYS%" "DAG%" "PART%" "EXEC%"
echo "───────────────────────────────────────────────────────────────────────────────────────────────────────────"

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    log="$LOG_DIR/${workload}__bs${block_size}.log"
    if [ ! -f "$log" ]; then
      printf "%-36s  %6s  NO LOG\n" "$workload" "$block_size"
      continue
    fi

    anlys_ms=$(extract_analysis_ms    "$log")
    dag_ms=$(extract_dag_ms           "$log")
    exec_ms=$(extract_scope_ms        "$log")
    commit_ms=$(extract_commit_work_ms "$log")
    mat_ms=$(extract_mat_tail_ms      "$log")
    total_ms=$(extract_overhead_total_ms "$log")
    part_ms=$(( anlys_ms + dag_ms ))

    [ "$total_ms" -eq 0 ] && total_ms=1

    anlys_pct=$(( anlys_ms * 100 / total_ms ))
    dag_pct=$(( dag_ms     * 100 / total_ms ))
    part_pct=$(( part_ms   * 100 / total_ms ))
    exec_pct=$(( exec_ms   * 100 / total_ms ))

    printf "%-36s  %6s  %8s  %6s  %7s  %7s  %7s  %6s  %8s  |  %5s%%  %4s%%  %5s%%  %5s%%\n" \
      "$workload" "$block_size" \
      "$anlys_ms" "$dag_ms" "$part_ms" "$exec_ms" "$commit_ms" "$mat_ms" "$total_ms" \
      "$anlys_pct" "$dag_pct" "$part_pct" "$exec_pct"
  done
  echo ""
done

# ── µs-resolution raw lines ───────────────────────────────────────────────────
echo ""
echo "═══════════════════════════════════════════════════════════════════════════════════════════════════════════"
echo " RAW TIMING LINES — exact values including µs resolution (ms rounds to 0 for small/fast phases)"
echo "═══════════════════════════════════════════════════════════════════════════════════════════════════════════"

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    log="$LOG_DIR/${workload}__bs${block_size}.log"
    [ -f "$log" ] || continue
    echo ""
    echo "── ${workload}  bs=${block_size} ──"
    grep -E "\[PARTITION-TIMING\]|\[DAG-TIMING\]|\[COMMITTER\]|\[DAG-OVERHEAD\]|mat_tail" "$log" | tail -8
  done
done