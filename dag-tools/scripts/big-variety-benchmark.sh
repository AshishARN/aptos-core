#!/usr/bin/env bash
# =============================================================================
# dag_benchmark.sh — Comprehensive Aptos executor benchmark harness
#
# Tests a wide variety of workloads × block sizes, emitting a structured
# summary at the end. Run from the aptos-core repo root after building with:
#   cargo build --release -p aptos-executor-benchmark
#
# Usage:
#   chmod +x dag_benchmark.sh
#   ./dag_benchmark.sh [--threads N] [--blocks N] [--skip-create]
#
# Results are written to:
#   $LOG_DIR/<executor>__<workload>__bs<block_size>.log  (raw output)
#   $LOG_DIR/SUMMARY.txt                                 (human-readable table)
#   $LOG_DIR/raw_results.tsv                             (machine-readable TSV)
# =============================================================================
set -euo pipefail

# ── Argument parsing ──────────────────────────────────────────────────────────
THREADS=64
BLOCKS=10           # blocks per run — more blocks = warmer cache, more stable TPS
SKIP_CREATE=0
NUM_ACCOUNTS=1000000
COOL_DOWN_SECS=15   # seconds to sleep between runs (lets thermals/DRAM settle)

while [[ $# -gt 0 ]]; do
  case "$1" in
    --threads)     THREADS="$2";    shift 2 ;;
    --blocks)      BLOCKS="$2";     shift 2 ;;
    --skip-create) SKIP_CREATE=1;   shift   ;;
    *) echo "Unknown argument: $1"; exit 1  ;;
  esac
done

# ── Paths ─────────────────────────────────────────────────────────────────────
BINARY="./target/release/aptos-executor-benchmark"
DATA_DIR="/tmp/dag-bench-db"
CKPT_DIR="/tmp/dag-bench-checkpoint"
LOG_DIR="/tmp/dag-bench-logs"
SUMMARY_FILE="$LOG_DIR/SUMMARY.txt"
RAW_RESULTS="$LOG_DIR/raw_results.tsv"

mkdir -p "$LOG_DIR" "$CKPT_DIR"

# ── Executor under test ───────────────────────────────────────────────────────
# Keep as "aptos-vm-with-block-stm" for the baseline run.
# Change to your DAG executor type for the DAG run.
EXECUTOR_TYPE="aptos-vm-with-block-stm"

# ── Block sizes to sweep ──────────────────────────────────────────────────────
BLOCK_SIZES=(10 100 1000 10000 100000)

# ── Workload matrix ───────────────────────────────────────────────────────────
# Format: "label|transaction-type-flag"
# label       → used in log filenames and the summary table
# txn-type    → passed to --transaction-type
#
# Conflict profile groupings:
#   ZERO-CONFLICT  : fully independent txns (best-case parallelism)
#   CONFLICT SWEEP : monotonic counter with N distinct hotspot accounts
#                    (tunes chain depth vs parallelism continuously)
#   RESOURCE MOD   : global resource reads/writes (non-trivial R/W sets)
#   DEFI           : swap-style workloads (user-deployed contracts)
#   FA / TOKEN     : fungible-asset and NFT minting
WORKLOADS=(
  # ── Zero-conflict (pure parallelism ceiling) ──────────────────────
  "no-conflict-coin|non-conflicting-coin-transfer"
  "no-conflict-fa|apt-fa-transfer"

  # ── Conflict sweep: monotonic counter, N hotspot accounts ─────────
  # N=1000 → ~100 independent chains of depth 100 per 100k-txn block
  # N=100  → ~10 chains of depth 1000
  # N=10   → ~10 chains of depth 10000 (near-serial)
  # N=1    → one global chain (fully serial)
  "conflict-1000-hot|monotonic-counter-multiple1000"
  "conflict-100-hot|monotonic-counter-multiple100"
  "conflict-10-hot|monotonic-counter-multiple10"
  "conflict-1-hot|monotonic-counter-single"

  # ── Global resource modification ──────────────────────────────────
  "modify-global|modify-global-resource"
  "modify-global-agg|modify-global-resource-agg-v2"

  # ── DeFi / swap (user-deployed contracts, non-trivial R/W sets) ───
  "lp-swap|liquidity-pool-swap"
  "lp-swap-stable|liquidity-pool-swap-stable"

  # ── FungibleAsset / NFT workloads ─────────────────────────────────
  # Note: these use a single admin sender — fundamentally serial chains.
  # DAG will serialize them correctly (via WAW on sender account);
  # Block-STM hides the latency via speculative pipelining.
  "nft-mint|token-v2-ambassador-mint"
  "fa-mint|fungible-asset-mint"
)

# ── Helpers ───────────────────────────────────────────────────────────────────

log()  { echo "[$(date '+%H:%M:%S')] $*"; }
sep()  { printf '%0.s═' {1..72}; echo; }
hsep() { printf '%0.s─' {1..72}; echo; }

# Extract the "Overall" TPS figure.
# The benchmark emits lines like:
#   "Overall: 23456 txns/s"  or  "Overall: 23456 TPS"
extract_tps() {
  local f="$1"
  grep -iE "^Overall" "$f" 2>/dev/null \
    | grep -oE '[0-9]+(\.[0-9]+)?' \
    | tail -1 \
    || echo "N/A"
}

# Extract partition-phase timing from DAG runs.
# Returns a compact single-line summary.
extract_dag_timing() {
  local f="$1"
  local waves part anal constr
  waves=$(grep  "PARTITION-TIMING.*waves"      "$f" 2>/dev/null | tail -1 \
            | grep -oE '[0-9]+ waves'          | head -1 || echo "?")
  part=$(grep   "PARTITION-TIMING.*total_part" "$f" 2>/dev/null | tail -1 \
            | grep -oP 'total_partition=\K[0-9]+' || echo "?")
  anal=$(grep   "PARTITION-TIMING.*static_ana" "$f" 2>/dev/null | tail -1 \
            | grep -oP 'static_analysis=\K[0-9]+'  || echo "?")
  constr=$(grep "PARTITION-TIMING.*dag_constr" "$f" 2>/dev/null | tail -1 \
            | grep -oP 'dag_construction=\K[0-9]+' || echo "?")
  echo "waves=${waves} part=${part}ms anal=${anal}ms constr=${constr}ms"
}

# Extract unknown-provenance read/write counts (DAG diagnostic).
extract_unknowns() {
  local f="$1"
  grep "DAG-DIAG.*unknown_reads" "$f" 2>/dev/null | tail -1 \
    | grep -oE 'unknown_reads=[0-9]+ unknown_writes=[0-9]+' \
    || echo "unknown_reads=? unknown_writes=?"
}

# Extract scope wall time (actual parallel execution window).
extract_scope_ms() {
  local f="$1"
  grep "DAG-TIMING.*scope_wall" "$f" 2>/dev/null | tail -1 \
    | grep -oP 'scope_wall=\K[0-9]+' \
    || echo "?"
}

# Extract committer stats.
extract_committer() {
  local f="$1"
  grep "COMMITTER" "$f" 2>/dev/null | tail -1 \
    | grep -oE 'commits=[0-9]+ wait_total=[0-9]+ms work_total=[0-9]+ms' \
    || echo ""
}

# ── DB creation ───────────────────────────────────────────────────────────────
sep
log "Aptos Executor Benchmark — executor: $EXECUTOR_TYPE"
log "Threads: $THREADS | Blocks per run: $BLOCKS | Accounts: $NUM_ACCOUNTS"
log "Block sizes: ${BLOCK_SIZES[*]}"
log "Workloads:   ${#WORKLOADS[@]} defined"
log "Cool-down:   ${COOL_DOWN_SECS}s between runs"
sep

if [[ $SKIP_CREATE -eq 1 ]]; then
  log "--skip-create: assuming DB exists at $DATA_DIR"
elif [[ -d "$DATA_DIR" ]]; then
  log "DB exists at $DATA_DIR — skipping creation (delete it to force a fresh one)"
else
  log "Creating benchmark DB ($NUM_ACCOUNTS accounts) → $DATA_DIR"
  "$BINARY" \
    --block-executor-type "$EXECUTOR_TYPE" \
    create-db \
    --data-dir "$DATA_DIR" \
    --num-accounts "$NUM_ACCOUNTS"
  log "DB creation complete."
fi

# ── Initialize output files ───────────────────────────────────────────────────
{
  echo "Executor: $EXECUTOR_TYPE | $(date)"
  hsep
  printf "%-28s  %-10s  %-12s  %s\n" "WORKLOAD" "BLOCK_SIZE" "TPS" "DAG_DIAG"
  hsep
} > "$SUMMARY_FILE"

# TSV header
printf "workload\tblock_size\texecutor\ttps\t"
printf "waves\tpartition_ms\tanalysis_ms\tconstruction_ms\t"
printf "unknown_reads\tunknown_writes\tscope_wall_ms\n" \
  > "$RAW_RESULTS"

# ── Benchmark loop ────────────────────────────────────────────────────────────
TOTAL_RUNS=$(( ${#WORKLOADS[@]} * ${#BLOCK_SIZES[@]} ))
RUN_NUM=0

for workload_entry in "${WORKLOADS[@]}"; do
  IFS='|' read -r label txn_type <<< "$workload_entry"

  for block_size in "${BLOCK_SIZES[@]}"; do
    RUN_NUM=$(( RUN_NUM + 1 ))
    log_file="$LOG_DIR/${EXECUTOR_TYPE}__${label}__bs${block_size}.log"

    sep
    log "[$RUN_NUM/$TOTAL_RUNS]  workload=$label  block_size=$block_size  txn_type=$txn_type"
    sep

    # Run the benchmark. Both stdout and stderr go to the log file AND terminal.
    set +e
    "$BINARY" \
      --block-executor-type "$EXECUTOR_TYPE" \
      --block-size "$block_size" \
      --execution-threads "$THREADS" \
      run-executor \
      --data-dir    "$DATA_DIR" \
      --checkpoint-dir "$CKPT_DIR" \
      --blocks      "$BLOCKS" \
      --main-signer-accounts "$NUM_ACCOUNTS" \
      --transaction-type "$txn_type" \
      2>&1 | tee "$log_file"
    EXIT_CODE=${PIPESTATUS[0]}
    set -e

    if [[ $EXIT_CODE -ne 0 ]]; then
      log "  ⚠ Run exited with code $EXIT_CODE — results may be incomplete"
    fi

    # ── Parse results ────────────────────────────────────────────────
    tps=$(extract_tps "$log_file")
    dag_timing=$(extract_dag_timing "$log_file")
    unknowns=$(extract_unknowns "$log_file")
    scope_ms=$(extract_scope_ms "$log_file")
    committer=$(extract_committer "$log_file")

    # Individual fields for TSV
    waves_val=$(echo "$dag_timing" | grep -oP 'waves=\K[^\s]+'  || echo "?")
    part_val=$(echo  "$dag_timing" | grep -oP 'part=\K[^\s]+'   || echo "?")
    anal_val=$(echo  "$dag_timing" | grep -oP 'anal=\K[^\s]+'   || echo "?")
    const_val=$(echo "$dag_timing" | grep -oP 'constr=\K[^\s]+' || echo "?")
    unk_r=$(echo "$unknowns" | grep -oP 'unknown_reads=\K[0-9]+'  || echo "?")
    unk_w=$(echo "$unknowns" | grep -oP 'unknown_writes=\K[0-9]+' || echo "?")

    # ── Append to files ──────────────────────────────────────────────
    printf "%-28s  %-10s  %-12s  %s  |  %s\n" \
      "$label" "$block_size" "$tps" "$dag_timing" "$unknowns" \
      >> "$SUMMARY_FILE"

    printf "%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n" \
      "$label" "$block_size" "$EXECUTOR_TYPE" "$tps" \
      "$waves_val" "$part_val" "$anal_val" "$const_val" \
      "$unk_r" "$unk_w" "$scope_ms" \
      >> "$RAW_RESULTS"

    # ── Quick inline summary ─────────────────────────────────────────
    log "  → TPS=$tps"
    [[ -n "$dag_timing"  ]] && log "  → $dag_timing"
    [[ -n "$unknowns"    ]] && log "  → $unknowns"
    [[ -n "$scope_ms"    ]] && log "  → scope_wall=${scope_ms}ms"
    [[ -n "$committer"   ]] && log "  → $committer"

    # ── Cool-down ────────────────────────────────────────────────────
    if [[ $RUN_NUM -lt $TOTAL_RUNS ]]; then
      log "  Cooling down ${COOL_DOWN_SECS}s..."
      sleep "$COOL_DOWN_SECS"
    fi
  done

  # Blank line between workload groups in summary
  echo "" >> "$SUMMARY_FILE"
done

# ── Final printed report ──────────────────────────────────────────────────────
sep
log "All $TOTAL_RUNS runs complete."
sep

echo ""
echo "══════════════════════════════════════════════════════════════════════════"
echo " FULL SUMMARY  (executor: $EXECUTOR_TYPE)"
echo " $(date)"
echo "══════════════════════════════════════════════════════════════════════════"
cat "$SUMMARY_FILE"

echo ""
echo "══════════════════════════════════════════════════════════════════════════"
echo " TPS PIVOT TABLE  (rows=workload, columns=block_size)"
echo "══════════════════════════════════════════════════════════════════════════"
printf "%-28s" "WORKLOAD"
for bs in "${BLOCK_SIZES[@]}"; do
  printf "  %10s" "BS=$bs"
done
echo ""
hsep
for workload_entry in "${WORKLOADS[@]}"; do
  IFS='|' read -r label _ <<< "$workload_entry"
  printf "%-28s" "$label"
  for bs in "${BLOCK_SIZES[@]}"; do
    lf="$LOG_DIR/${EXECUTOR_TYPE}__${label}__bs${bs}.log"
    if [[ -f "$lf" ]]; then
      tps=$(extract_tps "$lf")
    else
      tps="MISSING"
    fi
    printf "  %10s" "$tps"
  done
  echo ""
done

echo ""
echo "══════════════════════════════════════════════════════════════════════════"
echo " DAG DIAGNOSTICS PIVOT  (partition overhead per workload × block_size)"
echo " (all '?' values are normal for Block-STM runs — no DAG pass runs then)"
echo "══════════════════════════════════════════════════════════════════════════"
printf "%-28s  %-10s  %-30s  %s\n" "WORKLOAD" "BLOCK_SIZE" "TIMING" "UNKNOWNS"
hsep
for workload_entry in "${WORKLOADS[@]}"; do
  IFS='|' read -r label _ <<< "$workload_entry"
  for bs in "${BLOCK_SIZES[@]}"; do
    lf="$LOG_DIR/${EXECUTOR_TYPE}__${label}__bs${bs}.log"
    if [[ -f "$lf" ]]; then
      dt=$(extract_dag_timing "$lf")
      uk=$(extract_unknowns   "$lf")
      printf "%-28s  %-10s  %-30s  %s\n" "$label" "$bs" "$dt" "$uk"
    fi
  done
done

echo ""
echo "══════════════════════════════════════════════════════════════════════════"
echo " OUTPUT FILES"
echo "══════════════════════════════════════════════════════════════════════════"
echo "  Summary table  : $SUMMARY_FILE"
echo "  TSV (pandas/R) : $RAW_RESULTS"
echo "  Raw logs       : $LOG_DIR/"
echo ""
echo " To compare Block-STM vs DAG, run this script twice with different"
echo " EXECUTOR_TYPE values, then paste both raw_results.tsv files here"
echo " for delta analysis and charts."
echo "══════════════════════════════════════════════════════════════════════════"