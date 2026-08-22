// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Correctness pin for baking a direct `CALL` to a statically bound callee that
// declares its OWN exception table
// (`jit::direct_call_exc_table_publish_enabled`).
//
// The ban this lifts had one stated reason: *"a raw `CALL` has no Rust frame to
// notice the `i64::MIN` sentinel and run the callee's own handler."* That is a
// correctness claim, and it is the ONLY thing this probe exists to falsify.
// Every case below puts a handler in the CALLEE and checks it still runs, or
// still does not run, exactly as it does on HotSpot.
//
//   java                                          -cp out ExcTableDirectCallOracle
//   cratonvm --java-home <jdk>                    -cp out ExcTableDirectCallOracle
//   CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=0 cratonvm ... (the ban restored)
//
// All three must print byte-for-byte the same output.
//
// # The frame in the middle is load-bearing
//
// `driver` is not a wrapper for tidiness. The gate only applies to callees
// reached through `callee_compiler`, i.e. from an ORDINARY compiled frame — an
// OSR body emits its own invokes and never consults it
// (`direct callee binds: 0 bound, 0 left`). A probe whose hot loop called these
// methods directly would run entirely on the dispatch helper and pass without
// ever baking the direct `CALL` it claims to test. The loop calls `driver`;
// `driver` calls the guarded callees.
//
// Confirm the path is live rather than assuming it, with
// `CRATONVM_DBG=intrinsic-stats`:
//
//   ban lifted:  bind refused, callee-exception-table  ABSENT or unchanged
//   ban kept:    bind refused, callee-exception-table: N   (N > 0)
//
// A run where that counter does not move between arms is a run where this
// probe proved nothing.
public final class ExcTableDirectCallOracle {

    static final int[] ARR = { 11, 22, 33 };
    static long sink;

    // --- 1. the callee catches its own EXPLICIT throw ----------------------
    static String selfCatchExplicit(int i) {
        try {
            if (i >= 0) { throw new IllegalStateException("boom" + (i & 3)); }
            return "unreachable";
        } catch (IllegalStateException e) {
            return "caught:" + e.getMessage();
        }
    }

    // --- 2. the callee catches its own IMPLICIT trap -----------------------
    // This is the case the ban's stated reason is actually about: an implicit
    // AIOOBE/NPE/div-by-zero leaves through the deopt sentinel, not an athrow.
    static String selfCatchAioobe(int[] a, int idx) {
        try {
            return "val:" + a[idx];
        } catch (ArrayIndexOutOfBoundsException e) {
            return "aioobe";
        }
    }

    static String selfCatchNpe(int[] a) {
        try {
            return "len:" + a.length;
        } catch (NullPointerException e) {
            return "npe";
        }
    }

    static String selfCatchDivZero(int n, int d) {
        try {
            return "q:" + (n / d);
        } catch (ArithmeticException e) {
            return "arith";
        }
    }

    // --- 3. the callee's `finally` runs, then the throw propagates ---------
    static int finallyThenPropagate(int[] counter, int i) {
        try {
            throw new RuntimeException("prop" + (i & 3));
        } finally {
            counter[0]++;
        }
    }

    // --- 4. the callee's handler is the WRONG type and must not catch ------
    static int wrongHandlerType(int i) {
        try {
            return 10 / (i - i);
        } catch (IllegalStateException e) {
            return -1;
        }
    }

    // --- 5. callee A calls callee B; B throws, A catches -------------------
    static int innerThrower(int i) {
        throw new RuntimeException("t" + (i & 3));
    }

    static String nestedCatch(int i) {
        try {
            return "b:" + innerThrower(i);
        } catch (RuntimeException e) {
            return "A caught " + e.getMessage();
        }
    }

    // --- 6. catch one type, throw another from the handler -----------------
    static String rethrowDifferent(int i) {
        try {
            throw new IllegalArgumentException("inner" + (i & 3));
        } catch (IllegalArgumentException e) {
            throw new IllegalStateException("outer:" + e.getMessage());
        }
    }

    /**
     * The ordinary compiled frame the gate needs. Every guarded callee above is
     * reached from HERE, never from the loop in {@link #main}.
     */
    static long driver(int i, int[] counter) {
        long acc = 0;
        acc += selfCatchExplicit(i).length();
        acc += selfCatchAioobe(ARR, i & 7).length();
        acc += selfCatchNpe(((i & 1) == 0) ? null : ARR).length();
        acc += selfCatchDivZero(100, i & 1).length();
        try {
            acc += finallyThenPropagate(counter, i);
        } catch (RuntimeException e) {
            acc += e.getMessage().length();
        }
        try {
            acc += wrongHandlerType(i);
        } catch (ArithmeticException e) {
            acc += 7;
        }
        acc += nestedCatch(i).length();
        try {
            acc += rethrowDifferent(i).length();
        } catch (IllegalStateException e) {
            acc += e.getMessage().length();
        }
        return acc;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 60_000;
        int[] counter = new int[1];

        // Warm through `driver`, so `driver` tiers up as an ordinary callee and
        // its calls to the guarded methods are the ones the gate decides on.
        for (int i = 0; i < iters; i++) {
            sink += driver(i, counter);
        }

        System.out.println("== the callee's own handler ==");
        System.out.println("explicit    = " + selfCatchExplicit(5));
        System.out.println("aioobe hit  = " + selfCatchAioobe(ARR, 99));
        System.out.println("aioobe miss = " + selfCatchAioobe(ARR, 1));
        System.out.println("npe hit     = " + selfCatchNpe(null));
        System.out.println("npe miss    = " + selfCatchNpe(ARR));
        System.out.println("div hit     = " + selfCatchDivZero(100, 0));
        System.out.println("div miss    = " + selfCatchDivZero(100, 4));

        System.out.println("== finally, then propagate ==");
        int before = counter[0];
        String propagated;
        try {
            finallyThenPropagate(counter, 2);
            propagated = "NO EXCEPTION";
        } catch (RuntimeException e) {
            propagated = e.getMessage();
        }
        System.out.println("propagated  = " + propagated);
        System.out.println("finally ran = " + (counter[0] == before + 1));

        System.out.println("== the wrong handler type must not catch ==");
        String wrong;
        try {
            wrong = "returned " + wrongHandlerType(3);
        } catch (ArithmeticException e) {
            wrong = "ArithmeticException";
        }
        System.out.println("wrongType   = " + wrong);

        System.out.println("== nested and rethrow ==");
        System.out.println("nested      = " + nestedCatch(6));
        String rethrown;
        try {
            rethrown = "returned " + rethrowDifferent(1);
        } catch (IllegalStateException e) {
            rethrown = e.getMessage();
        }
        System.out.println("rethrow     = " + rethrown);

        System.out.println("== totals ==");
        System.out.println("counter     = " + counter[0]);
        System.out.println("sink        = " + sink);
    }
}
