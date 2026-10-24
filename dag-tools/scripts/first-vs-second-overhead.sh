#!/usr/bin/env bash
set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
BINARY="cargo run --release -p aptos-executor-benchmark --"
DATA_DIR="/tmp/dag-bench-db"
CHECKPOINT_DIR="/tmp/dag-bench-checkpoint"
LOG_DIR="/tmp/dag-warmup-logs"
BLOCKS=5  # Running 5 blocks to get past the setup phase
SLEEP_SECS=15

BLOCK_SIZES=(100)

WORKLOADS=(
  #"liquidity-pool-swap"
  #"token-v2-ambassador-mint"
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

# ── Extraction functions ───────────────────────────────────────────────────────
extract_analysis_ms() { grep -oP '\[PARTITION-TIMING\].*static_analysis=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }
extract_hits()        { grep -oP '\[PARTITION-TIMING\].*hits=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }
extract_misses()      { grep -oP '\[PARTITION-TIMING\].*misses=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }
extract_dag_ms()      { grep -oP '\[PARTITION-TIMING\].*dag_construction=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }
extract_exec_ms()     { grep -oP '\[DAG-TIMING\].*scope_wall=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }
extract_total_ms()    { grep -oP '\[DAG-OVERHEAD\].*total_ms=\K[0-9]+' "$1" | sed -n "${2}p" || echo "0"; }

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
echo "Step 2: Running workloads for 5 BLOCKS to capture warmup..."
echo ""

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    log_file="$LOG_DIR/${workload}__bs${block_size}.log"
    echo "Running $workload (bs=$block_size) ..."
    $BINARY \
      --block-executor-type "$EXECUTOR_TYPE" \
      --block-size "$block_size" \
      run-executor \
      --blocks "$BLOCKS" \
      --transaction-type "$workload" \
      --data-dir "$DATA_DIR" \
      --checkpoint-dir "$CHECKPOINT_DIR" \
      > "$log_file" 2>&1
    sleep "$SLEEP_SECS"
  done
done

# ── Summary ────────────────────────────────────────────────────────────────────
echo ""
echo "════════════════════════════════════════════════════════════════════════════════════════════════════"
echo " COLD VS WARM BLOCK PROGRESSION (BLOCKS 1 to 5)"
echo "════════════════════════════════════════════════════════════════════════════════════════════════════"
printf "%-28s %6s %4s | %8s %8s %8s | %6s %7s %8s\n" \
  "WORKLOAD" "BS" "BLK" "MISSES" "HITS" "ANLYS_ms" "DAG_ms" "EXEC_ms" "TOTAL_ms"
echo "────────────────────────────────────────────────────────────────────────────────────────────────────"

for workload in "${WORKLOADS[@]}"; do
  for block_size in "${BLOCK_SIZES[@]}"; do
    log="$LOG_DIR/${workload}__bs${block_size}.log"
    [ -f "$log" ] || continue

    # NOW LOOPING THROUGH ALL 5 BLOCKS
    for blk in 1 2 3 4 5; do
      misses=$(extract_misses "$log" "$blk")
      hits=$(extract_hits "$log" "$blk")
      anlys_ms=$(extract_analysis_ms "$log" "$blk")
      dag_ms=$(extract_dag_ms "$log" "$blk")
      exec_ms=$(extract_exec_ms "$log" "$blk")
      total_ms=$(extract_total_ms "$log" "$blk")

    #   # Skip printing if the line is completely empty/0 (means block didn't run via DAG)
    #   if [ "$misses" -eq 0 ] && [ "$hits" -eq 0 ] && [ "$anlys_ms" -eq 0 ]; then
    #      printf "%-28s %6s %4s | %37s\n" "$workload" "$block_size" "$blk" "[Skipped / Publish Block]"
    #      continue
    #   fi

      printf "%-28s %6s %4s | %8s %8s %8s | %6s %7s %8s\n" \
        "$workload" "$block_size" "$blk" \
        "$misses" "$hits" "$anlys_ms" "$dag_ms" "$exec_ms" "$total_ms"
    done
    echo "  --------------------------------------------------------------------------------------------------"
  done
done
echo ""