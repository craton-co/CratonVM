// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 34 (orchestrator): the cost of creating and
// catching an exception, which the wave-34 hidden-frame filter sits on
// (docs/internal/fixed-bugs/interpreter-L4-cross-loader-type-checks-and-trace-shapes-FIXED-20261001.md,
// item 4). Every `--jdk-only` throwable capture now asks, per frame, whether
// the frame is hidden; after the first capture of a method that is one load.
//
// Rows (ns per throw-and-catch, stderr):
//
//   throw-d2    a RuntimeException thrown two frames below the catch
//   throw-d20   the same twenty frames below the catch (twenty frames to
//               judge per capture)
//   create-d20  `new RuntimeException()` at depth 20, not thrown (the capture
//               alone)
//   trace-d20   the same, then `getStackTrace().length` (materialised)
//
// Stdout prints a checksum per row, identical on every VM.
//
// Run: javac -d out L7W34ThrowableCostBench.java && cratonvm --java-home <jdk25> [--nojit] -cp out L7W34ThrowableCostBench
public class L7W34ThrowableCostBench {
    static int sink;

    static int down(int depth, int mode) {
        if (depth > 0) {
            return down(depth - 1, mode) + 1;
        }
        switch (mode) {
            case 0 -> throw new RuntimeException("x");
            case 1 -> {
                RuntimeException e = new RuntimeException("y");
                return e.hashCode() & 1;
            }
            default -> {
                RuntimeException e = new RuntimeException("z");
                return e.getStackTrace().length;
            }
        }
    }

    static long row(String name, int depth, int mode, int reps) {
        long sum = 0;
        long t0 = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            try {
                sum += down(depth, mode);
            } catch (RuntimeException e) {
                sum += e.getMessage().length();
            }
        }
        long ns = System.nanoTime() - t0;
        System.out.println(name + " " + (mode == 1 ? reps : sum));
        System.err.printf(java.util.Locale.ROOT, "%-12s %8.1f ns/op%n", name, (double) ns / reps);
        return sum;
    }

    public static void main(String[] args) {
        for (int warm = 0; warm < 2; warm++) {
            row("warm-d2", 2, 0, 20_000);
            row("warm-d20", 20, 0, 20_000);
        }
        row("throw-d2", 2, 0, 200_000);
        row("throw-d20", 20, 0, 100_000);
        row("create-d20", 20, 1, 100_000);
        row("trace-d20", 20, 2, 50_000);
    }
}
