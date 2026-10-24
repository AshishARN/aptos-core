#!/usr/bin/env python3
import json
import struct
import random
import argparse

def u64_to_bcs_hex(val: int) -> str:
    return struct.pack("<Q", val).hex()

def get_hotspot_receiver(n_total: int, n_hot: int, prob: float) -> int:
    if random.random() < prob:
        return random.randint(0, n_hot - 1)
    else:
        return random.randint(n_hot, n_total - 1)

def main():
    parser = argparse.ArgumentParser(description="Generate exact DAG topologies")
    parser.add_argument("--shape", type=str, required=True, choices=["zero", "waterfall", "segmented", "hotspot"])
    parser.add_argument("--txns", type=int, default=10000)
    parser.add_argument("--chains", type=int, default=100, help="For segmented: number of parallel chains")
    parser.add_argument("--hotspot-prob", type=float, default=0.8, help="For hotspot: probability of hitting the hot group")
    parser.add_argument("--output", type=str, default="/tmp/custom_block.json")
    args = parser.parse_args()

    block = []
    
    # We offset receivers by 50,000 to ensure they never overlap with our sender indices (1 to 10,000)
    # This guarantees we don't accidentally create WAW sender sequence number bottlenecks.
    RECEIVER_OFFSET = 50000

    for i in range(args.txns):
        sender_idx = i + 1  # 1 to 10000 (unique sender for every txn to allow max parallelism)
        
        if args.shape == "zero":
            # ZERO CONFLICT: Every sender sends to a unique receiver
            receiver_idx = RECEIVER_OFFSET + i
            
        elif args.shape == "waterfall":
            # WATERFALL: Every sender sends to the EXACT SAME receiver
            # DAG sees 10,000 WAW conflicts on this one receiver's CoinStore
            receiver_idx = RECEIVER_OFFSET
            
        elif args.shape == "segmented":
            # SEGMENTED (Grid): Senders send to a fixed number of 'pool' receivers
            # e.g., 100 chains means 100 independent sequential lines of 100 txns each.
            chain_id = i % args.chains
            receiver_idx = RECEIVER_OFFSET + chain_id
            
        elif args.shape == "hotspot":
            # HOTSPOT: 80% of txns go to 20% of receivers.
            # Simulates real-world DeFi where a few pools get most of the traffic.
            total_receivers = 1000
            hot_receivers = int(total_receivers * 0.2)
            recv = get_hotspot_receiver(total_receivers, hot_receivers, args.hotspot_prob)
            receiver_idx = RECEIVER_OFFSET + recv

        block.append({
            "sender_idx": sender_idx,
            "module_address": "0x1",
            "module_name": "aptos_account",
            "function_name": "transfer",
            "ty_args": [],
            "args_bcs": [
                f"ACCOUNT:{receiver_idx}",
                u64_to_bcs_hex(1)
            ]
        })

    with open(args.output, "w") as f:
        json.dump(block, f, indent=2)
    
    print(f"✅ Generated {args.shape.upper()} shape with {args.txns} txns.")

if __name__ == "__main__":
    main()