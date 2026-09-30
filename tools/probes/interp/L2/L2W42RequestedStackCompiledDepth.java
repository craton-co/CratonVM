// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L2: a thread that asked for a 64 MiB
// stack runs a compiled recursion 400 000 levels deep
// (docs/internal/fixed-bugs/interpreter-L2-a-requested-stack-carrier-is-sized-for-interpreted-frames-RETIRED-20261006.md).
//
// The interpreter grants a request its own frame budget (8192 frames per
// MiB), so `--nojit` held the depth; compiled code is bounded by the
// carrier's real bytes, and the carrier was `request + 8 MiB` although a
// compiled frame is about 4x HotSpot's. The recursion is warmed first on a
// shallow depth so the deep run is compiled. On `dev` before the fix the
// carrier was 72 MiB (a self-call budget of 68 MiB): 400 000 levels fit only
// below about 178 bytes a level, and the i41-L2 page measured 220-440, so
// `deep=SOE` is the expected base answer (not observed: run it on the base).
// After the fix the carrier is 264 MiB.
//
// HotSpot 25 prints (and the same with -Xint):
//     warm=ok
//     deep=400000
//     small-thread=SOE
// `small-thread` is a control: a 1 MiB request (the carrier keeps the 8 MiB
// default) must still overflow at 2 000 000 levels, on HotSpot too.
public class L2W42RequestedStackCompiledDepth {
    static int down(int n) {
        return n == 0 ? 0 : 1 + down(n - 1);
    }

    static String run(long stackBytes, int depth) throws Exception {
        String[] out = new String[1];
        Thread t = new Thread(null, () -> {
            try {
                out[0] = Integer.toString(down(depth));
            } catch (StackOverflowError e) {
                out[0] = "SOE";
            }
        }, "deep", stackBytes);
        t.start();
        t.join();
        return out[0];
    }

    public static void main(String[] args) throws Exception {
        int sum = 0;
        for (int i = 0; i < 20_000; i++) {
            sum += down(100);
        }
        System.out.println("warm=" + (sum == 2_000_000 ? "ok" : "bad " + sum));
        System.out.println("deep=" + run(64L << 20, 400_000));
        System.out.println("small-thread=" + run(1L << 20, 2_000_000));
    }
}
