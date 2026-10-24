# DAG Benchmark & Workload Tools

This directory contains benchmarking harnesses, DAG topology generators, and workload simulation scripts for evaluating DAG-based transaction scheduling against Block-STM in Aptos.

---

## Directory Overview

```
dag-tools/
├── scripts/                    # Shell benchmark runners across diverse workloads
│   ├── benchmark.sh            # Basic conflict benchmark runner
│   ├── variety-benchmark.sh    # Multi-workload benchmark with block sizes & cooldowns
│   ├── big-variety-benchmark.sh# Comprehensive multi-workload × block-size harness
│   ├── fa-benchmark.sh         # Fungible Asset workload benchmark
│   ├── pareto-conflicts.sh     # Pareto distribution conflict benchmark
│   ├── first-vs-second-overhead.sh # Cold vs warm / multi-block warmup overhead tests
│   └── overhead-log.sh         # Detailed overhead logging for block execution
├── dag-shape-benchmarks/       # DAG topology shape generator & runner
│   ├── generate_shape.py       # Synthesizes block transactions with specific DAG structures
│   └── run_shapes.sh           # Runs benchmarks across Zero Conflict, Waterfall, Segmented, Hotspot
├── dag-metrics-benchmark/      # Isolated DAG metric evaluations (depth, density, degree)
│   ├── generate_metrics.py     # Generates DAG transactions controlling specific graph metrics
│   └── run_metrics.sh          # Evaluates performance impact of metric levels (low, med, high)
├── produce-txn-csvs/           # CSV-based DAG structure producers
│   ├── pareto-dag.py           # Pareto conflict distribution DAG generator
│   ├── fanout-chain-fanin.py   # Fan-out / Chain / Fan-in DAG generator
│   ├── make-fanin.py           # Fan-in pattern generator
│   └── produce-binary-dag.py   # Binary tree DAG generator
└── produce-txn-json/           # JSON-based block generators and mainnet replays
    ├── generate-json-block.py  # Generates custom JSON transaction blocks
    ├── build-mainnet-replay.py # Formats historical mainnet blocks into replay payloads
    ├── fetch-mainnet.py        # Fetches actual mainnet block transactions from node API
    └── deploy-and-test.py      # Deployment testing helper
```

---

## Prerequisites & Building

1. Ensure Rust toolchain and dependencies are available.
2. Build the Aptos executor benchmark binary in release mode:
   ```bash
   cargo build --release -p aptos-executor-benchmark
   ```
   The binary is produced at `target/release/aptos-executor-benchmark`.

3. Initialize the benchmark database (e.g. 1M or 2M accounts):
   ```bash
   cargo run --release -p aptos-executor-benchmark -- \
     --block-executor-type aptos-vm-with-block-stm create-db \
     --data-dir /tmp/dag-bench-db \
     --num-accounts 1000000
   ```

---

## Workload Overrides

The modified `aptos-executor-benchmark` supports injecting custom DAG structures via environment variables:

1. **`CUSTOM_DAG_FILE`**:
   Points to a CSV file specifying transaction dependencies and conflict structures.
   ```bash
   CUSTOM_DAG_FILE="/tmp/metric_dag.csv" cargo run --release -p aptos-executor-benchmark -- ...
   ```

2. **`CUSTOM_JSON_FILE`**:
   Points to a JSON file containing transaction payload definitions (sender, module, function, arguments).
   ```bash
   CUSTOM_JSON_FILE="/tmp/custom_block.json" cargo run --release -p aptos-executor-benchmark -- ...
   ```

---

## Running Benchmarks

### 1. DAG Topology Shapes (`dag-shape-benchmarks`)
Evaluates how different transaction dependency graphs behave:
- **Zero Conflict**: Independent transactions (maximum concurrency)
- **Waterfall**: Strictly sequential linear dependencies
- **Segmented Chains**: Multiple parallel independent linear dependency chains
- **Hotspot**: High probability contention on a small set of accounts

```bash
cd dag-tools/dag-shape-benchmarks
# (or from repo root: cd dag-shape-benchmarks)
./run_shapes.sh
```

### 2. DAG Metric Isolations (`dag-metrics-benchmark`)
Isolates individual graph properties (depth, density, degree) to measure their impact on waves and TPS:
```bash
cd dag-tools/dag-metrics-benchmark
# (or from repo root: cd dag-metrics-benchmark)
./run_metrics.sh
```

### 3. General Workload Benchmarks (`scripts/`)
Run multi-workload tests across various block sizes (100 to 10,000 txns):
```bash
# Run from repository root
./dag-tools/scripts/variety-benchmark.sh
# Or if running the root copy:
./variety-benchmark.sh
```
