import subprocess
import time
import os

JAVAC = "/data/toolchain/jdk-25/bin/javac"
HOTSPOT = "/data/toolchain/jdk-25/bin/java"
CVM = "/data/CVM/target/release/cratonvm"

os.makedirs("/tmp/cratonbench_build", exist_ok=True)

bench_java = """
import java.util.HashMap;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

public class CratonBenchParam {
    public static long benchArithmetic(long iterations) {
        long sum = 0;
        for (long i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }

    public static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }

    public static int sieveReps(int limit, int reps) {
        int count = 0;
        for (int r = 0; r < reps; r++) {
            boolean[] composite = new boolean[limit + 1];
            for (int i = 2; i * i <= limit; i++) {
                if (!composite[i]) {
                    for (int j = i * i; j <= limit; j += i) {
                        composite[j] = true;
                    }
                }
            }
            int c = 0;
            for (int i = 2; i <= limit; i++) {
                if (!composite[i]) c++;
            }
            count = c;
        }
        return count;
    }

    public static int matrixKernel(int size) {
        int[][] a = new int[size][size];
        int[][] b = new int[size][size];
        int[][] c = new int[size][size];
        for (int i = 0; i < size; i++) {
            for (int j = 0; j < size; j++) {
                a[i][j] = i + j;
                b[i][j] = i - j;
            }
        }
        for (int i = 0; i < size; i++) {
            for (int j = 0; j < size; j++) {
                int sum = 0;
                for (int k = 0; k < size; k++) {
                    sum += a[i][k] * b[k][j];
                }
                c[i][j] = sum;
            }
        }
        return c[0][0] + c[size - 1][size - 1];
    }

    public static long hashmapKernel(int n) {
        HashMap<Integer, Integer> map = new HashMap<>(n);
        for (int i = 0; i < n; i++) {
            map.put(i, i * 2);
        }
        long sum = 0;
        for (int i = 0; i < n; i++) {
            Integer val = map.get(i);
            if (val != null) sum += val;
        }
        return sum;
    }

    public static long stringRegexKernel(int n) {
        StringBuilder sb = new StringBuilder(n * 8);
        for (int i = 1; i <= n; i++) {
            sb.append(i).append(' ');
        }
        String text = sb.toString();
        Pattern p = Pattern.compile("(\\\\d+)");
        Matcher m = p.matcher(text);
        long sum = 0;
        while (m.find()) {
            sum += Long.parseLong(m.group(1));
        }
        return sum;
    }

    static class TreeNode {
        TreeNode left, right;
        TreeNode(TreeNode left, TreeNode right) {
            this.left = left;
            this.right = right;
        }
        int check() {
            if (left == null) return 1;
            return 1 + left.check() + right.check();
        }
    }

    static TreeNode bottomUpTree(int depth) {
        if (depth > 0) {
            return new TreeNode(bottomUpTree(depth - 1), bottomUpTree(depth - 1));
        } else {
            return new TreeNode(null, null);
        }
    }

    public static int btreesKernel(int maxDepth) {
        int minDepth = 4;
        int stretchDepth = maxDepth + 1;
        int check = bottomUpTree(stretchDepth).check();

        TreeNode longLivedTree = bottomUpTree(maxDepth);
        int totalCheck = 0;

        for (int depth = minDepth; depth <= maxDepth; depth += 2) {
            int iterations = 1 << (maxDepth - depth + minDepth);
            for (int i = 1; i <= iterations; i++) {
                totalCheck += bottomUpTree(depth).check();
            }
        }
        totalCheck += longLivedTree.check();
        return totalCheck + check;
    }

    public static void main(String[] args) {
        String phase = args[0];
        long t0 = System.currentTimeMillis();
        long checksum = 0;
        if (phase.equals("arithmetic")) {
            checksum = benchArithmetic(Long.parseLong(args[1]));
        } else if (phase.equals("fib")) {
            checksum = fib(Integer.parseInt(args[1]));
        } else if (phase.equals("sieve")) {
            checksum = sieveReps(Integer.parseInt(args[1]), Integer.parseInt(args[2]));
        } else if (phase.equals("matrix")) {
            checksum = matrixKernel(Integer.parseInt(args[1]));
        } else if (phase.equals("hashmap")) {
            checksum = hashmapKernel(Integer.parseInt(args[1]));
        } else if (phase.equals("stringregex")) {
            checksum = stringRegexKernel(Integer.parseInt(args[1]));
        } else if (phase.equals("btrees")) {
            checksum = btreesKernel(Integer.parseInt(args[1]));
        }
        long ms = System.currentTimeMillis() - t0;
        System.out.println("TIME_MS:" + ms + " CHECKSUM:" + checksum);
    }
}
"""

with open("/tmp/cratonbench_build/CratonBenchParam.java", "w") as f:
    f.write(bench_java)

subprocess.run([JAVAC, "/tmp/cratonbench_build/CratonBenchParam.java"], check=True)

configs = [
    ("cvm_zgc", CVM, ["--compatible", "-XX:+UseZGC"]),
    ("cvm_g1", CVM, ["--compatible", "-XX:+UseG1GC"]),
    ("hs_zgc", HOTSPOT, ["-XX:+UseZGC"]),
    ("hs_g1", HOTSPOT, ["-XX:+UseG1GC"]),
]

benchmarks = [
    {
        "name": "Arithmetic",
        "phase": "arithmetic",
        "rows": [
            ("1B", ["1000000000"]),
            ("1.5B", ["1500000000"]),
            ("2B", ["2000000000"]),
            ("2.4B", ["2400000000"]),
            ("3B", ["3000000000"]),
        ],
        "xmx": "-Xmx8g"
    },
    {
        "name": "Fibonacci",
        "phase": "fib",
        "rows": [
            ("40", ["40"]),
            ("42", ["42"]),
            ("44", ["44"]),
            ("46", ["46"]),
            ("48", ["48"]),
        ],
        "xmx": "-Xmx8g"
    },
    {
        "name": "Sieve",
        "phase": "sieve",
        "rows": [
            ("50K × 10K", ["50000", "10000"]),
            ("50K × 20K", ["50000", "20000"]),
            ("100K × 20K", ["100000", "20000"]),
            ("100K × 40K", ["100000", "40000"]),
            ("100K × 60K", ["100000", "60000"]),
        ],
        "xmx": "-Xmx8g"
    },
    {
        "name": "Matrix",
        "phase": "matrix",
        "rows": [
            ("720x720", ["720"]),
            ("960x960", ["960"]),
            ("1280x1280", ["1280"]),
            ("1920x1920", ["1920"]),
            ("2560x2560", ["2560"]),
        ],
        "xmx": "-Xmx16g"
    },
    {
        "name": "Binary Trees",
        "phase": "btrees",
        "rows": [
            ("16", ["16"]),
            ("18", ["18"]),
            ("20", ["20"]),
            ("22", ["22"]),
            ("24", ["24"]),
        ],
        "xmx": "-Xmx16g"
    },
    {
        "name": "HashMap",
        "phase": "hashmap",
        "rows": [
            ("1M", ["1000000"]),
            ("5M", ["5000000"]),
            ("10M", ["10000000"]),
            ("50M", ["50000000"]),
            ("100M", ["100000000"]),
        ],
        "xmx": "-Xmx24g"
    },
    {
        "name": "String / Regex",
        "phase": "stringregex",
        "rows": [
            ("100K", ["100000"]),
            ("500K", ["500000"]),
            ("1M", ["1000000"]),
            ("5M", ["5000000"]),
            ("10M", ["10000000"]),
        ],
        "xmx": "-Xmx16g"
    }
]

full_results = {}

print("=== STARTING COMPLETE CRATONBENCH SUITE (7 BENCHMARKS, 35 SCENARIOS) ===\n")

for b in benchmarks:
    b_name = b["name"]
    phase = b["phase"]
    xmx = b["xmx"]
    print(f"==================================================")
    print(f"Running Benchmark Group: {b_name}")
    print(f"==================================================")
    full_results[b_name] = []

    for label, args in b["rows"]:
        print(f"\n--- Scenario: {label} ---")
        row_res = {"label": label}
        for cfg_id, bin_path, gc_flags in configs:
            cmd = [bin_path] + gc_flags + [xmx, "-cp", "/tmp/cratonbench_build", "CratonBenchParam", phase] + args
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
                print(f"  [{cfg_id:<8}] FAIL / ERROR! exit={res.returncode}")
                if res.stdout:
                    print(f"  stdout: {res.stdout.strip()[:200]}")
                if res.stderr:
                    print(f"  stderr: {res.stderr.strip()[:200]}")
                row_res[cfg_id] = None
            else:
                print(f"  [{cfg_id:<8}] {ms:>7} ms (checksum: {checksum})")
                row_res[cfg_id] = ms
        full_results[b_name].append(row_res)
    print()

import json
with open("/data/CVM/cratonbench_full_results.json", "w") as f:
    json.dump(full_results, f, indent=2)

print("\n=== BENCHMARK SUITE COMPLETE — RESULTS SAVED TO /data/CVM/cratonbench_full_results.json ===")
