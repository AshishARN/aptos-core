#!/usr/bin/env python3

NUM_CHAINS = 64
CHAIN_LENGTH = 100
OUTPUT_FILE = "/tmp/complex_dag.csv"

def generate_dag():
    with open(OUTPUT_FILE, "w") as f:
        # ---------------------------------------------------------
        # PHASE 1: FAN-OUT
        # Account 0 batch-transfers to Accounts 1 through 64.
        # This forces Accounts 1..64 to wait for Tx 1.
        # ---------------------------------------------------------
        fanout_receivers = ",".join(str(c) for c in range(1, NUM_CHAINS + 1))
        f.write(f"0,{fanout_receivers}\n")
        
        # ---------------------------------------------------------
        # PHASE 2: 64 PARALLEL CHAINS (100 nodes deep)
        # We loop by step first (interleaving) so that all 64 chains 
        # execute side-by-side in perfectly balanced parallel waves.
        # ---------------------------------------------------------
        for step in range(CHAIN_LENGTH):
            for c in range(1, NUM_CHAINS + 1):
                sender = c
                receiver = 100 + c  # Send to an isolated receiver pool
                f.write(f"{sender},{receiver}\n")
        
        # ---------------------------------------------------------
        # PHASE 3: FAN-IN
        # Account 0 batch-transfers to Accounts 101 through 164.
        # Because the final step of all 64 chains wrote to these 
        # exact 64 receivers, this transaction will draw exactly 
        # 64 incoming edges, waiting for all chains to finish!
        # ---------------------------------------------------------
        fanin_receivers = ",".join(str(100 + c) for c in range(1, NUM_CHAINS + 1))
        f.write(f"0,{fanin_receivers}\n")

if __name__ == "__main__":
    generate_dag()
    total_txns = 1 + (NUM_CHAINS * CHAIN_LENGTH) + 1
    print(f"✅ Generated {total_txns} transactions in {OUTPUT_FILE}")
    print(f"   Run with: --block-size {total_txns}")