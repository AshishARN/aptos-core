with open("/tmp/tree_dag.csv", "w") as f:
    for i in range(1, 5000):
        # Parent -> Left Child
        f.write(f"{i},{i*2}\n")
        # Parent -> Right Child
        f.write(f"{i},{i*2 + 1}\n")