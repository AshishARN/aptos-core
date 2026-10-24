#!/usr/bin/env python3

import random
import argparse
import sys

def get_pareto_account(alpha, max_accounts):
    """
    Returns an account index heavily skewed towards 0 using a Pareto distribution.
    random.paretovariate(alpha) returns a float >= 1.0.
    We subtract 1 so our highest-probability account is 0.
    """
    # Generate the pareto number
    raw_val = random.paretovariate(alpha) - 1.0
    
    # Scale and wrap into our account space
    acc_index = int(raw_val * 10) % max_accounts
    return acc_index

def main():
    parser = argparse.ArgumentParser(description="Generate a DAG CSV with a specific Pareto conflict rate.")
    parser.add_argument("--txns", type=int, default=10000, help="Number of transactions to generate")
    parser.add_argument("--accounts", type=int, default=10000, help="Number of available accounts")
    parser.add_argument("--alpha", type=float, default=2.0, help="Pareto shape parameter. Lower = MORE conflicts. Higher = LESS conflicts.")
    parser.add_argument("--output", type=str, default="/tmp/pareto_dag.csv", help="Output CSV file path")
    args = parser.parse_args()

    # Track how many times each account is touched just to print a diagnostic summary
    touch_counts = {}

    with open(args.output, "w") as f:
        for _ in range(args.txns):
            sender = get_pareto_account(args.alpha, args.accounts)
            receiver = get_pareto_account(args.alpha, args.accounts)
            
            # Ensure sender != receiver
            while receiver == sender:
                receiver = get_pareto_account(args.alpha, args.accounts)
            
            # Write standard transfer edge
            f.write(f"{sender},{receiver}\n")
            
            # Update diagnostics
            touch_counts[sender] = touch_counts.get(sender, 0) + 1
            touch_counts[receiver] = touch_counts.get(receiver, 0) + 1

    # Sort accounts by most touched to show the user the conflict concentration
    sorted_touches = sorted(touch_counts.items(), key=lambda x: x[1], reverse=True)
    top_5 = sorted_touches[:5]
    total_touches = args.txns * 2 # Sender and Receiver
    
    print(f"✅ Generated {args.txns} txns in {args.output} (Alpha = {args.alpha})")
    print(f"   Top 5 Hottest Accounts:")
    for acc, count in top_5:
        percentage = (count / total_touches) * 100
        print(f"      Account {acc}: {count} accesses ({percentage:.1f}%)")

if __name__ == "__main__":
    main()