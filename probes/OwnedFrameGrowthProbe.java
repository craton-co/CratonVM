// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Does the number of `Owned` interpreter frames GROW with work, or is it a
// fixed startup cost?
//
// This decides whether boxing `OwnedFrameMeta` is safe. Boxing trades one heap
// allocation per `Owned` frame for 64 bytes off EVERY frame, so it is correct
// only while `Owned` stays rare in absolute terms.
//
// The counter reported owned=466 on a probe doing 8,000,000 calls and owned=478
// on a mixed indy/reflection/exception program doing ~1,600 — which reads as
// 30% there and 0.007% here. Two readings of the same near-constant. If `Owned`
// is a fixed VM-bootstrap cost, the share is an artifact of how little the
// small program does, and boxing is free on anything real.
//
// The alternative is that `Owned` is driven by the OPERATIONS this program
// happens to perform — lambdas, reflection, exception unwinding — in which case
// a long-running workload doing those in a loop would keep paying, and the
// share would stay high.
//
// So: do exactly those operations, in a loop, ITERS times. Run at two very
// different ITERS and compare the owned counts. Flat means startup. Growing
// means per-operation, and the boxing needs a different shape.
import java.lang.reflect.Method;
import java.util.function.IntUnaryOperator;

public final class OwnedFrameGrowthProbe {

    static int target(int x) { return x + 1; }

    public static void main(String[] args) throws Exception {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        int acc = 0;

        // invokedynamic: a lambda call site, invoked repeatedly.
        IntUnaryOperator f = x -> x + 1;

        // Reflection: a resolved Method, invoked repeatedly.
        Method m = OwnedFrameGrowthProbe.class.getDeclaredMethod("target", int.class);

        for (int i = 0; i < iters; i++) {
            acc += f.applyAsInt(i);
            acc += (Integer) m.invoke(null, i);
            try {
                if ((i & 1023) == 0) {
                    throw new IllegalStateException("unwind");
                }
            } catch (IllegalStateException e) {
                acc += e.getMessage().length();
            }
        }
        System.out.println("iters=" + iters + " guard=" + acc);
    }
}
