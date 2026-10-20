// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, lane L4: HotSpot performs no tail-call elimination, so a
// self-recursive tail call keeps one frame per activation. CratonVM's
// `invoke.rs` "8. Tail-call elimination" rewrites the caller frame in place for
// a SELF-recursive call followed by a matching return (`Frame::
// reset_for_tail_call`). See
// docs/internal/fixed-bugs/interpreter-L4-self-recursive-tail-call-elimination-diverges-from-hotspot-FIXED-20260923.md
//
// Expected on HotSpot 25 (any -Xss that cannot hold 5,000,000 frames):
//   trace delta = 50
//   soe = true
//   deep enough = true
//
// With the elimination active, `trace delta` reads 0 (the 50 collapsed frames
// are missing from getStackTrace) and the recursion runs to LIMIT without ever
// overflowing: `soe = false`. Both depend on which invoke path dispatches the
// call (the fast doors push ordinary frames), so a 50 on one run and a 0 under
// --nojit is itself a finding. Deterministic; runs in well under a second on
// HotSpot and a few seconds at worst on an interpreter that eliminates.

public class TailCallFidelityProbe {
    static final int LIMIT = 5_000_000;
    static int depth;

    // iload_0; ifne; ...; ireturn / iload_0 iconst_1 isub invokestatic ireturn
    static int traceLen(int n) {
        if (n == 0) {
            return new Throwable().getStackTrace().length;
        }
        return traceLen(n - 1);
    }

    // getstatic; iconst_1; iadd; putstatic; ...; invokestatic down; return
    static void down() {
        depth++;
        if (depth >= LIMIT) {
            return;
        }
        down();
    }

    public static void main(String[] a) {
        // Warm both shapes a little so every tier has seen them.
        for (int k = 0; k < 3; k++) {
            traceLen(10);
        }
        int base = traceLen(0);
        int deep = traceLen(50);
        System.out.println("trace delta = " + (deep - base));

        boolean soe = false;
        depth = 0;
        try {
            down();
        } catch (StackOverflowError e) {
            soe = true;
        }
        System.out.println("soe = " + soe);
        System.out.println("deep enough = " + (depth > 1000));
    }
}
