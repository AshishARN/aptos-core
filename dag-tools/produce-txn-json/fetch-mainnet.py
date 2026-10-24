import json
import struct
import requests

# The two blocks you want to extract
BLOCK_HEIGHTS = [79432868]
NODE_URL = "https://fullnode.mainnet.aptoslabs.com/v1"

# Global map to translate real mainnet addresses to our local benchmark accounts
address_to_idx = {}

def get_account_idx(address: str) -> int:
    """Maps a real mainnet address to a sequential integer for our sandbox."""
    # Standardize address format
    clean_addr = "0x" + address.replace("0x", "").zfill(64)
    if clean_addr not in address_to_idx:
        address_to_idx[clean_addr] = len(address_to_idx)
    return address_to_idx[clean_addr]

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

def heuristic_bcs_encode(arg) -> str:
    """
    Because the REST API returns human-readable JSON, we use heuristics 
    to encode arguments back to BCS hex for our Rust engine.
    """
    if isinstance(arg, bool):
        return "01" if arg else "00"
    
    if isinstance(arg, str):
        if arg.startswith("0x"):
            clean_hex = arg.replace("0x", "")
            
            # --- THE FIX ---
            # If the hex string has an odd length (because Aptos dropped the leading zero),
            # pad it with a zero so bytes.fromhex() can parse it properly.
            if len(clean_hex) % 2 != 0:
                clean_hex = "0" + clean_hex
                
            # If it's exactly 64 chars long (32 bytes), we assume it's an Address.
            if len(clean_hex) == 64:
                return f"ACCOUNT:{get_account_idx(arg)}"
            else:
                # Otherwise, assume it's a vector<u8> (bytes). Needs ULEB128 length prefix.
                b = bytes.fromhex(clean_hex)
                return (uleb128(len(b)) + b).hex()
                
        elif arg.isdigit():
            # Aptos returns u64 and u128 as strings. We default to u64 (8 bytes LE).
            return struct.pack("<Q", int(arg)).hex()
        else:
            # Standard string
            b = arg.encode('utf-8')
            return (uleb128(len(b)) + b).hex()
            
    if isinstance(arg, int):
        # Small integers (usually u8 or u32, we pack as u64 to be safe, but Move is strict)
        return struct.pack("<B", arg).hex()
        
    if isinstance(arg, list):
        # Array of items. ULEB length prefix + items
        res = uleb128(len(arg)).hex()
        for item in arg:
            encoded = heuristic_bcs_encode(item)
            if encoded.startswith("ACCOUNT:"):
                idx = int(encoded.split(":")[1])
                dummy = f"{idx:064x}" 
                res += dummy
            else:
                res += encoded
        return res

    return ""

def encode_vector_u8(data: bytes) -> str:
    return (uleb128(len(data)) + data).hex()

def fetch_and_convert_block(height: int):
    print(f"\nFetching Mainnet Block {height}...")
    url = f"{NODE_URL}/blocks/by_height/{height}?with_transactions=true"
    response = requests.get(url)
    
    if response.status_code != 200:
        print(f"Failed to fetch block: {response.text}")
        return

    data = response.json()
    transactions = data.get("transactions", [])
    
    sandbox_txns = []

# Inside fetch_and_convert_block(height):
    for tx in transactions:
        if tx["type"] != "user_transaction":
            continue
            
        payload = tx.get("payload", {})
        if payload.get("type") != "entry_function_payload":
            continue
            
        # Parse Module and Function
        func_parts = payload["function"].split("::")
        if len(func_parts) != 3:
            continue
            
        module_address, module_name, function_name = func_parts
        
        # 1. EXTRACT DATA FROM PAYLOAD FIRST
        sender = tx.get("sender", "")
        sender_idx = get_account_idx(sender)
        
        ty_args = payload.get("type_arguments", [])
        raw_args = payload.get("arguments", [])
        args_bcs = [heuristic_bcs_encode(arg) for arg in raw_args]
        
        # 2. --- THE MAINNET TO MOCK ROUTER ---
        if module_name == "router" and function_name == "swap_exact_input":
            module_address = "ACCOUNT:0"
            module_name = "mock_mainnet"
            # FORCE both type arguments to be AptosCoin so the local VM linker succeeds!
            ty_args = ["0x1::aptos_coin::AptosCoin", "0x1::aptos_coin::AptosCoin"]
            args_bcs = [struct.pack("<Q", 100).hex(), struct.pack("<Q", 100).hex()]
            
        elif module_name == "controller" and function_name == "deposit":
            module_address = "ACCOUNT:0"
            module_name = "mock_mainnet"
            # FORCE type argument to be AptosCoin
            ty_args = ["0x1::aptos_coin::AptosCoin"]
            args_bcs = [struct.pack("<Q", 100).hex()]
            
        elif module_name == "shared_pricer" and function_name == "update_price_2":
            module_address = "ACCOUNT:0"
            module_name = "mock_mainnet"
            ty_args = []
            args_bcs = [encode_vector_u8(b"mock_data")]
            
        elif module_name == "just" and function_name == "do_it":
            module_address = "ACCOUNT:0"
            module_name = "mock_mainnet"
            ty_args = []
            args_bcs = [encode_vector_u8(b"mock_data")]
        # -----------------------------------
        
        sandbox_txns.append({
            "sender_idx": sender_idx,
            "module_address": module_address,
            "module_name": module_name,
            "function_name": function_name,
            "ty_args": ty_args,
            "args_bcs": args_bcs
        })

    output_file = f"/tmp/mainnet_block_{height}.json"
    with open(output_file, "w") as f:
        json.dump(sandbox_txns, f, indent=2)
        
    print(f"✅ Extracted {len(sandbox_txns)} user transactions to {output_file}")
    print(f"📊 Unique Accounts mapped: {len(address_to_idx)}")

if __name__ == "__main__":
    for h in BLOCK_HEIGHTS:
        fetch_and_convert_block(h)
    print("\nExtraction Complete! You can now pass these JSON files to your DAG benchmark.")