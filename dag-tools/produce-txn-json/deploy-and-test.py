import json
import struct

def u64_to_bcs_hex(val: int) -> str:
    return struct.pack("<Q", val).hex()

def uleb128(n: int) -> bytes:
    res = bytearray()
    while True:
        byte = n & 0x7F
        n >>= 7
        if n:
            res.append(byte | 0x80)
        else:
            res.append(byte)
            break
    return bytes(res)

def encode_vector_u8(data: bytes) -> str:
    return (uleb128(len(data)) + data).hex()

def encode_vector_vector_u8(vec_of_bytes: list[bytes]) -> str:
    res = uleb128(len(vec_of_bytes))
    for data in vec_of_bytes:
        res += uleb128(len(data)) + data
    return res.hex()

with open("../../my_custom_module/build/MyBenchmarkModule/package-metadata.bcs", "rb") as f:
    metadata_bytes = f.read()

with open("../../my_custom_module/build/MyBenchmarkModule/bytecode_modules/counter.mv", "rb") as f:
    module_bytes = f.read()

block = []

# ========================================================
# BLOCK 1: Publish + Init + 98 Dummy Transfers
# (This block skips the DAG and commits instantly via BlockSTM)
# ========================================================
block.append({
    "sender_idx": 0,
    "module_address": "0x1",
    "module_name": "code",
    "function_name": "publish_package_txn",
    "ty_args": [],
    "args_bcs": [
        encode_vector_u8(metadata_bytes),
        encode_vector_vector_u8([module_bytes])
    ]
})

block.append({
    "sender_idx": 0,
    "module_address": "ACCOUNT:0", 
    "module_name": "counter",
    "function_name": "init",
    "ty_args": [],
    "args_bcs": []
})

# Fill the rest of Block 1 with independent dummy transfers
for i in range(2, 100):
    block.append({
        "sender_idx": i,
        "module_address": "0x1",
        "module_name": "aptos_account",
        "function_name": "transfer",
        "ty_args": [],
        "args_bcs": [
            "ACCOUNT:1", 
            u64_to_bcs_hex(1)
        ]
    })

# ========================================================
# BLOCK 2: The Actual DAG Workload
# 100 aggressive increments on Account 0's custom contract!
# ========================================================
for _ in range(100):
    block.append({
        "sender_idx": 0,
        "module_address": "ACCOUNT:0",
        "module_name": "counter",
        "function_name": "increment",
        "ty_args": [],
        "args_bcs": []
    })

with open("/tmp/custom_block.json", "w") as f:
    json.dump(block, f, indent=2)

print("✅ Generated 200 txns in /tmp/custom_block.json")