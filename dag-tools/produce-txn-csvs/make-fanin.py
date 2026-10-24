with open("/tmp/fanin_dag.csv", "w") as f:
    # Tx 1-9: Independent transactions touching Accounts 1-9
    for i in range(1, 10):
        f.write(f"{i},{i}\n")
    
    # Tx 10: Batch Transfer sending to Accounts 1 through 9
    receivers = ",".join([str(i) for i in range(1, 10)])
    f.write(f"0,{receivers}\n")