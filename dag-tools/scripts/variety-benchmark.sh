#!/usr/bin/env bash
set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
BINARY="cargo run --release -p aptos-executor-benchmark --"
DATA_DIR="/tmp/dag-bench-db"
CHECKPOINT_DIR="/tmp/dag-bench-checkpoint"
LOG_DIR="/tmp/dag-bench-logs"
BLOCKS=10
SLEEP_SECS=10

BLOCK_SIZES=(100 10000)

WORKLOADS=(
   "non-conflicting-coin-transfer"
  # "monotonic-counter-multiple1000"
  # "monotonic-counter-multiple100"
  # "monotonic-counter-multiple10"
  # "monotonic-counter-single"
  # "apt-fa-transfer"
   "liquidity-pool-swap"
  # "liquidity-pool-swap-stable"
  # "modify-global-resource"
  # "modify-global-resource-agg-v2"
  # "token-v1nft-mint-and-store-sequential"
  # "token-v1nft-mint-and-store-parallel"
  #  "token-v2-ambassador-mint"
  # "fungible-asset-mint"
  # "order-book-balanced-matches25-pct1-market"  
)

EXECUTOR_TYPES=(
  "aptos-vm-with-block-stm"
  # Add your DAG executor type name here if it differs from block-stm
  # e.g. "aptos-vm-dag" — check your --block-executor-type options
)

mkdir -p "$LOG_DIR"

# ── Helper ────────────────────────────────────────────────────────────────────
run_workload() {
  local executor_type="$1"
  local workload="$2"
  local block_size="$3"
  local log_file="$LOG_DIR/${executor_type}__${workload}__bs${block_size}.log"

  echo ""
  echo "════════════════════════════════════════════════════════"
  echo " executor   : $executor_type"
  echo " workload   : $workload"
  echo " block_size : $block_size"
  echo " log        : $log_file"
  echo "════════════════════════════════════════════════════════"

  $BINARY \
    --block-executor-type "$executor_type" \
    --block-size "$block_size" \
    run-executor \
    --blocks "$BLOCKS" \
    --transaction-type "$workload" \
    --data-dir "$DATA_DIR" \
    --checkpoint-dir "$CHECKPOINT_DIR" \
    2>&1 | tee "$log_file"
}

extract_result() {
  local log_file="$1"
  # Grab the Overall txn/s line that follows the PERFORMANCE METRICS header
  grep -A 5 "Overall" "$log_file" | grep "txn/s" | tail -1
}

extract_dag_diag() {
  local log_file="$1"
  # Last occurrence of the DAG diagnostic lines (final block is most stable).
  # We added DAG-OVERHEAD to catch the exec_scope_ms and total_ms at the end of the block.
  grep -E "PARTITION-TIMING|DAG-DIAG.*waves|DAG-DIAG.*unknown|DAG-DIAG.*in-degree|DAG-OVERHEAD" \
    "$log_file" | tail -8
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
echo "Step 2: Running benchmarks"
echo ""

for workload in "${WORKLOADS[@]}"; do
  for executor_type in "${EXECUTOR_TYPES[@]}"; do
    for block_size in "${BLOCK_SIZES[@]}"; do
      run_workload "$executor_type" "$workload" "$block_size"

      echo ""
      echo "  [Sleeping ${SLEEP_SECS}s to cool down...]"
      sleep "$SLEEP_SECS"
    done
  done
done

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════════════════════════"
echo " RESULTS SUMMARY"
echo "════════════════════════════════════════════════════════════════════════════════════"
printf "%-30s  %-40s  %-12s  %s\n" "EXECUTOR" "WORKLOAD" "BLOCK_SIZE" "TXN/S"
echo "────────────────────────────────────────────────────────────────────────────────────"

for workload in "${WORKLOADS[@]}"; do
  for executor_type in "${EXECUTOR_TYPES[@]}"; do
    for block_size in "${BLOCK_SIZES[@]}"; do
      log_file="$LOG_DIR/${executor_type}__${workload}__bs${block_size}.log"
      if [ -f "$log_file" ]; then
        txns=$(extract_result "$log_file" | awk '{print $NF}')
        printf "%-30s  %-40s  %-12s  %s\n" "$executor_type" "$workload" "$block_size" "$txns"
      else
        printf "%-30s  %-40s  %-12s  %s\n" "$executor_type" "$workload" "$block_size" "NO LOG"
      fi
    done
    echo ""
  done
done

echo ""
echo "════════════════════════════════════════════════════════════════════════════════════"
echo " DAG DIAGNOSTICS (last block per workload × block_size)"
echo "════════════════════════════════════════════════════════════════════════════════════"

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    dag_log="$LOG_DIR/aptos-vm-with-block-stm__${workload}__bs${block_size}.log"
    if [ -f "$dag_log" ]; then
      echo ""
      echo "── $workload  block_size=$block_size ──"
      extract_dag_diag "$dag_log"
    fi
  done
done