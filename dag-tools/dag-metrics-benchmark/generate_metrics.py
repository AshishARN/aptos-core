#!/usr/bin/env python3
import argparse

def main():
    parser = argparse.ArgumentParser(description="Generate DAGs isolated by specific graph metrics")
    parser.add_argument("--metric", choices=["depth", "density", "degree"], required=True, 
                        help="depth = Critical Path & Width, density = Edges/Txn, degree = Max Hotspot")
    parser.add_argument("--level", choices=["low", "med", "high"], required=True)
    parser.add_argument("--txns", type=int, default=10000)
    parser.add_argument("--output", type=str, default="/tmp/metric_dag.csv")
    args = parser.parse_args()

    txns = args.txns
    
    # We offset receivers by 50,000 to ensure they never overlap with our senders.
    # This ensures sequence number conflicts (WAW on the sender account) do not 
    # unintentionally distort the receiver topologies we are trying to test.
    OFFSET = 50000 
    lines = []

    # -------------------------------------------------------------------------
    # METRIC 1 & 2: Critical Path Length (Depth) & Avg Txns per Wave (Width)
    # We control this by determining how many transactions write to the same 
    # receiver. If 100 txns hit the same receiver, they form a chain of depth 100.
    # -------------------------------------------------------------------------
    if args.metric == "depth":
        if args.level == "low":
            chunk_size = 1         # Depth 1, Avg Width 10000 (Pure Parallel)
        elif args.level == "med":
            chunk_size = 100       # Depth 100, Avg Width 100 (100 parallel chains)
        else: # high
            chunk_size = txns      # Depth 10000, Avg Width 1 (Waterfall)
        
        for i in range(txns):
            sender = i
            receiver = OFFSET + (i // chunk_size)
            lines.append(f"{sender},{receiver}")

    # -------------------------------------------------------------------------
    # METRIC 3: Edge Density (Edges / Total Transactions)
    # We control this using your BATCH TRANSFER feature. One transaction 
    # writes to multiple unique receivers.
    # -------------------------------------------------------------------------
    elif args.metric == "density":
        if args.level == "low":
            batch = 1              # 1 edge per txn
        elif args.level == "med":
            batch = 10             # 10 edges per txn
        else: # high
            batch = 100            # 100 edges per txn
        
        # Lower the offset to give maximum room for receivers without hitting senders (0-9999)
        OFFSET = 10000 
        num_accounts = 1000000
        receiver_pool_size = num_accounts - OFFSET
        
        for i in range(txns):
            sender = i
            # Generate `batch` number of unique receivers for this sender.
            # We safely wrap around WITHIN the receiver pool so we NEVER hit a sender index.
            receivers = [str(OFFSET + ((i * batch + k) % receiver_pool_size)) for k in range(batch)]
            lines.append(f"{sender},{','.join(receivers)}")

    # -------------------------------------------------------------------------
    # METRIC 4: Max Degree (Hotspot / Fan-In Bottleneck)
    # We control this by forcing a specific number of transactions to target
    # the exact same receiver (Node 0), while the rest execute in parallel.
    # -------------------------------------------------------------------------
    elif args.metric == "degree":
        if args.level == "low":
            hot_count = 10         # Max In-Degree = 10
        elif args.level == "med":
            hot_count = 1000       # Max In-Degree = 1,000
        else: # high
            hot_count = 10000      # Max In-Degree = 10,000
        
        for i in range(txns):
            sender = i
            if i < hot_count:
                receiver = OFFSET  # Hit the hotspot!
            else:
                receiver = OFFSET + i # Safe, unique parallel transfer
            lines.append(f"{sender},{receiver}")

    with open(args.output, "w") as f:
        f.write("\n".join(lines) + "\n")
    
    print(f"✅ Generated {args.metric.upper()} ({args.level.upper()}) | {txns} txns -> {args.output}")

if __name__ == "__main__":
    main()