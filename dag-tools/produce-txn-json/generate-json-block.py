import json
import struct

def u64_to_bcs_hex(val: int) -> str:
    # BCS for u64 is 8 bytes, little-endian
    return struct.pack("<Q", val).hex()

block = []

# Transaction 0: Standard aptos_account transfer (No Type Args)
block.append({
    "sender_idx": 0,
    "module_address": "0x1",
    "module_name": "aptos_account",
    "function_name": "transfer",
    "ty_args": [],
    "args_bcs": [
        "ACCOUNT:1",          # Dynamically resolved to Account 1
        u64_to_bcs_hex(1000)  # Amount
    ]
})

# Transaction 1: Generic Coin transfer (Requires Type Args)
# This tests the `parse_type_tag` logic we added to Rust!
block.append({
    "sender_idx": 2,
    "module_address": "0x1",
    "module_name": "coin",
    "function_name": "transfer",
    "ty_args": [
        "0x1::aptos_coin::AptosCoin"  # The generic type T
    ],
    "args_bcs": [
        "ACCOUNT:3",          # Dynamically resolved to Account 3
        u64_to_bcs_hex(500)   # Amount
    ]
})

# Transaction 2: Another generic Coin transfer
block.append({
    "sender_idx": 4,
    "module_address": "0x1",
    "module_name": "coin",
    "function_name": "transfer",
    "ty_args": [
        "0x1::aptos_coin::AptosCoin"
    ],
    "args_bcs": [
        "ACCOUNT:5",
        u64_to_bcs_hex(250)
    ]
})

with open("/tmp/custom_block.json", "w") as f:
    json.dump(block, f, indent=2)

print("✅ Generated /tmp/custom_block.json")

"""CUSTOM_JSON_FILE="/tmp/custom_block.json" cargo run --release -p aptos-executor-benchmark -- \
  --block-executor-type aptos-vm-with-block-stm \
  --block-size 100 \
  run-executor \
  --blocks 5 \
  --data-dir /tmp/dag-bench-db \
  --checkpoint-dir /tmp/dag-bench-checkpoint
"""