##creating db
# remove existing db: rm -rf /tmp/benchmark-db /tmp/benchmark-checkpoint
# creating db: ./target/release/aptos-executor-benchmark --block-executor-type aptos-vm-with-block-stm create-db --data-dir /tmp/dag-bench-db --num-accounts 1000000


#!/bin/bash
# DATA_DIR=/tmp/benchmark-db
# CKPT_DIR=/tmp/benchmark-checkpoint
DATA_DIR="/tmp/dag-bench-db"
CKPT_DIR="/tmp/dag-bench-checkpoint"
mkdir -p $CKPT_DIR

RESULTS_FILE="./conflict_benchmark_results.txt"
> $RESULTS_FILE  # clear previous results

NUM_ACCOUNTS=1000000
BLOCK_SIZE=100000
THREADS=64

run_bench() {
  local label=$1
  local txn_type=$2
  echo "===== Running: $label ($txn_type) =====" | tee -a $RESULTS_FILE
  
  ./target/release/aptos-executor-benchmark \
    --block-executor-type aptos-vm-with-block-stm \
    --block-size $BLOCK_SIZE \
    --execution-threads $THREADS \
    run-executor \
    --data-dir $DATA_DIR \
    --checkpoint-dir $CKPT_DIR \
    --blocks 10 \
    --main-signer-accounts $NUM_ACCOUNTS \
    --transaction-type $txn_type 2>&1 | tee /tmp/bench_run.log

  echo "--- $label ---" >> $RESULTS_FILE
  awk '/^Overall/,/FeeStatement/' /tmp/bench_run.log | tail -20 >> $RESULTS_FILE
  echo "" >> $RESULTS_FILE
}

echo "Starting Block-STM Conflict Sweep on $THREADS threads..."

#Sweep from 0% Conflict up to 100% Conflict
run_bench "0-Conflict_Perfect-Parallel" "non-conflicting-coin-transfer"
# run_bench "Low-Conflict_1000-Hotspots" "monotonic-counter-multiple1000"
run_bench "Med-Conflict_100-Hotspots" "monotonic-counter-multiple100"
run_bench "High-Conflict_10-Hotspots" "monotonic-counter-multiple10"
run_bench "100-Conflict_1-Hotspot" "monotonic-counter-single"

echo ""
echo "========== SUMMARY =========="
cat $RESULTS_FILE