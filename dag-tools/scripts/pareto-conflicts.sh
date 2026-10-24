#!/usr/bin/env bash

#CUSTOM_DAG_FILE="/tmp/tree_dag.csv" ./pareto-conflicts.sh


set -euo pipefail

# ── Config ────────────────────────────────────────────────────────────────────
BINARY="cargo run --release -p aptos-executor-benchmark --"
DATA_DIR="/tmp/dag-bench-db"
CHECKPOINT_DIR="/tmp/dag-bench-checkpoint"
LOG_DIR="/tmp/pareto-logs"
BLOCK_SIZE=10000
BLOCKS=5
SLEEP_SECS=5

# Alpha values: 1.0 (Extreme Conflict) to 5.0 (Near Zero Conflict)
ALPHAS=(1.0 1.5 2.0 3.0 5.0)

mkdir -p "$LOG_DIR"

echo "=========================================================="
echo " Starting Pareto Conflict Sweep..."
echo "=========================================================="

for ALPHA in "${ALPHAS[@]}"; do
    log_file="$LOG_DIR/pareto_alpha_${ALPHA}.log"
    
    echo "1. Generating DAG CSV for Alpha = $ALPHA..."
    python3 produce-txn-csvs/pareto-dag.py --txns $BLOCK_SIZE --accounts 100000 --alpha $ALPHA > /dev/null
    
    echo "2. Running Benchmark for Alpha = $ALPHA (saving to $log_file)..."
    CUSTOM_DAG_FILE="/tmp/pareto_dag.csv" $BINARY \
        --block-executor-type aptos-vm-with-block-stm \
        --block-size $BLOCK_SIZE \
        run-executor \
        --blocks $BLOCKS \
        --data-dir "$DATA_DIR" \
        --checkpoint-dir "$CHECKPOINT_DIR" > "$log_file" 2>&1
        
    echo "   Done. Sleeping for ${SLEEP_SECS}s to cool down hardware."
    echo "----------------------------------------------------------"
    sleep $SLEEP_SECS
done

# ── Summary ───────────────────────────────────────────────────────────────────
echo ""
echo "══════════════════════════════════════════════════════════════════════════════════════"
echo " PARETO CONFLICT SWEEP RESULTS"
echo "══════════════════════════════════════════════════════════════════════════════════════"
printf "%-10s | %-12s | %-10s | %-10s | %-20s\n" "ALPHA" "TXN/S" "WAVES" "DAG_MS" "MAX CONFLICTS"
echo "──────────────────────────────────────────────────────────────────────────────────────"

for ALPHA in "${ALPHAS[@]}"; do
    log_file="$LOG_DIR/pareto_alpha_${ALPHA}.log"
    
    if [ -f "$log_file" ]; then
        # Extract TPS
        tps=$(grep -A 5 "Overall" "$log_file" | grep "txn/s" | tail -1 | awk '{print $NF}' || echo "N/A")
        
        # Extract Waves
        waves=$(grep -oP 'waves=\K[0-9]+' "$log_file" | tail -1 || echo "N/A")
        
        # Extract DAG Construction Time
        dag_ms=$(grep -oP 'dag_construction=\K[0-9]+' "$log_file" | tail -1 || echo "N/A")
        
        # Extract the highest in-degree (to show how heavily bottlenecked the hottest account was)
        max_conflict=$(grep "in-degree histogram" "$log_file" | tail -1 | grep -oP '\(\d+, \d+\)' | tail -1 || echo "N/A")

        printf "%-10s | %-12s | %-10s | %-10s | %-20s\n" "$ALPHA" "$tps" "$waves" "$dag_ms" "$max_conflict"
    else
        printf "%-10s | %-12s | %-10s | %-10s | %-20s\n" "$ALPHA" "NO LOG" "-" "-" "-"
    fi
done
echo ""