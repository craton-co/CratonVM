import subprocess
import time
import os

JAVAC = "/data/toolchain/jdk-25/bin/javac"
HOTSPOT = "/data/toolchain/jdk-25/bin/java"
CVM = "/data/CVM/target/release/cratonvm"

iterations_map = {
    "1B": 1000000000,
    "1.5B": 1500000000,
    "2B": 2000000000,
    "2.4B": 2400000000,
    "3B": 3000000000,
}

iterations_keys = ["1B", "1.5B", "2B", "2.4B", "3B"]

configs = [
    ("CratonVM ZGC", CVM, ["-XX:+UseZGC"]),
    ("CratonVM G1", CVM, ["-XX:+UseG1GC"]),
    ("HotSpot ZGC", HOTSPOT, ["-XX:+UseZGC"]),
    ("HotSpot G1", HOTSPOT, ["-XX:+UseG1GC"]),
]

runner_java = """
public class ArithmeticRunner {
    static long benchArithmetic(long iterations) {
        long sum = 0;
        for (long i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    public static void main(String[] args) {
        long iter = Long.parseLong(args[0]);
        long t0 = System.currentTimeMillis();
        long checksum = benchArithmetic(iter);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("TIME_MS:" + elapsed + " CHECKSUM:" + checksum);
    }
}
"""

os.makedirs("/tmp/cratonbench_build", exist_ok=True)
with open("/tmp/cratonbench_build/ArithmeticRunner.java", "w") as f:
    f.write(runner_java)

subprocess.run([JAVAC, "/tmp/cratonbench_build/ArithmeticRunner.java"], check=True)

results = {key: {} for key in iterations_keys}

print("Starting Interleaved Arithmetic Benchmark Runs...\n")
for key in iterations_keys:
    iters = iterations_map[key]
    print(f"=== Iterations: {key} ({iters:,} ops) ===")
    for name, bin_path, gc_flags in configs:
        cmd = [bin_path] + gc_flags + ["-cp", "/tmp/cratonbench_build", "ArithmeticRunner", str(iters)]
        res = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        
        output = res.stdout + "\n" + res.stderr
        
        ms = None
        checksum = None
        for line in output.splitlines():
            line_str = line.strip()
            if "TIME_MS:" in line_str:
                parts = line_str.split()
                for p in parts:
                    if p.startswith("TIME_MS:"):
                        ms = int(p.split(":")[1])
                    elif p.startswith("CHECKSUM:"):
                        checksum = p.split(":")[1]
        
        if ms is None:
            print(f"  [{name}] FAILED / ERROR! exit={res.returncode}")
            if res.stdout:
                print(f"  stdout: {res.stdout.strip()[:200]}")
            if res.stderr:
                print(f"  stderr: {res.stderr.strip()[:200]}")
            results[key][name] = "FAIL"
        else:
            print(f"  [{name}] -> {ms} ms (checksum: {checksum})")
            results[key][name] = ms
    print()

print("\n================ FINAL BENCHMARK RESULTS MATRIX ================")
header = f"{'Iterations':<12} | " + " | ".join(f"{cfg[0]:<14}" for cfg in configs)
print(header)
print("-" * len(header))

for key in iterations_keys:
    row = f"{key:<12} | "
    row_vals = []
    for cfg in configs:
        val = results[key].get(cfg[0], "N/A")
        val_str = f"{val} ms" if isinstance(val, int) else str(val)
        row_vals.append(f"{val_str:<14}")
    row += " | ".join(row_vals)
    print(row)

