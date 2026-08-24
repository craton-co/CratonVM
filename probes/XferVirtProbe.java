// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * Prices the compiled-caller -> INTERPRETED-callee transition for an
 * `invokevirtual` / `invokeinterface` callee — the half
 * `performance/jit-compiled-caller-to-interpreted-callee-FIXED-20260823.md`
 * left open after its `invokestatic` fix.
 *
 * `probes/XferProbe.java` is the static counterpart and cannot answer this:
 * `try_jit_static_bytecode_callee` serves its call site, so the static probe
 * measures a path that is already fixed. This one's callee is an INSTANCE
 * method reached through an interface-typed field, so the site is
 * `invokeinterface` and its receiver is one concrete class — the monomorphic
 * shape a real application's call graph is mostly made of.
 *
 * Three arms, ONE binary:
 *
 *   virt                                       compiled caller -> compiled callee
 *   CRATONVM_JIT_DENY=XferVirtProbe$Impl.step  compiled caller -> INTERPRETED callee
 *   --nojit                                    both interpreted (the control)
 *
 * The middle row is the one that matters, and the third is what makes it
 * readable: if compiled->interpreted is SLOWER than interpreted->interpreted,
 * the transition is costing more than the compilation is buying.
 *
 *   XferVirtProbe [iterations]
 */
public class XferVirtProbe {

    interface Step {
        int step(int x);
    }

    static final class Impl implements Step {
        public int step(int x) {
            return x + 1;
        }
    }

    static long sink;
    static Step target = new Impl();

    static void loop(int n) {
        for (int i = 0; i < n; i++) {
            sink += target.step(i);
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2000000;
        loop(50000);
        long t0 = System.nanoTime();
        loop(n);
        long d = System.nanoTime() - t0;
        System.out.printf("xfervirt %8.1f ns/op sink=%d%n", (double) d / n, sink);
        System.out.flush();
        Runtime.getRuntime().halt(0);
    }
}
