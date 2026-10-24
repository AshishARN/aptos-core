#!/bin/bash
DATA_DIR=/tmp/benchmark-db
CKPT_DIR=/tmp/benchmark-checkpoint
mkdir -p $CKPT_DIR

RESULTS_FILE="./fa_benchmark_results.txt"
> $RESULTS_FILE  # clear previous results

# Set this to the exact number of accounts you used during the 'create-db' step!
NUM_ACCOUNTS=1000000 
# Default block size in Aptos benchmarks is usually 10000, explicitly defining it here for clarity
BLOCK_SIZE=10000

run_bench() {
  local label=$1
  shift
  echo "===== Running: $label =====" | tee -a $RESULTS_FILE
  # capture output, print it live, then extract Overall block
  "$@" 2>&1 | tee /tmp/bench_run.log
  echo "--- $label ---" >> $RESULTS_FILE
  # extract from "Overall" header to the last Events line
  awk '/^Overall/,/FeeStatement/' /tmp/bench_run.log | tail -20 >> $RESULTS_FILE
  echo "" >> $RESULTS_FILE
}

# Sweep across 1, 4, 16, and 32 threads to measure Block-STM parallel scaling
for threads in 1 4 16 32; do
  run_bench "apt-fa-transfer-${threads}t" \
    ./target/release/aptos-executor-benchmark \
    --block-executor-type aptos-vm-with-block-stm \
    --block-size $BLOCK_SIZE \
    --execution-threads $threads \
    run-executor \
    --data-dir $DATA_DIR \
    --checkpoint-dir $CKPT_DIR \
    --blocks 200 \
    --main-signer-accounts $NUM_ACCOUNTS \
    --transaction-type apt-fa-transfer \
    --module-working-set-size 1
done

echo ""
echo "========== SUMMARY =========="
cat $RESULTS_FILE