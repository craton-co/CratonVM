// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * gci i18/dk -- the cost of a root deposit's frame class owners
 * ({@code vm/src/memory/roots.rs} {@code frame_class_owners}) with many
 * non-strong hidden classes alive. Defines {@code hidden} non-strong hidden
 * classes ({@code Lookup.defineHiddenClass} without {@code STRONG}, from the
 * bytes of {@code GciNt16Payload}, which must be in the same class dir), then
 * times a virtual thread that calls {@code Thread.yield()} {@code yields}
 * times per round: every yield unmounts the continuation, and the VM deposits
 * the thread's roots, frame class owners included.
 *
 * <p>Usage: {@code GciDk18OwnerDepositProbe [hidden=500] [yields=20000] [rounds=7]}.
 * stdout is deterministic; the per-yield timings go to stderr
 * ({@code [gci-dk18] ... ns_per_yield_median=}).
 */
public final class GciDk18OwnerDepositProbe {
    static final List<Class<?>> KEEP = new ArrayList<>();
    static volatile long acc;

    public static void main(String[] args) throws Exception {
        int hidden = args.length > 0 ? Integer.parseInt(args[0]) : 500;
        int yields = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 7;
        byte[] payload;
        try (InputStream in =
                GciDk18OwnerDepositProbe.class.getResourceAsStream("/GciNt16Payload.class")) {
            payload = in.readAllBytes();
        }
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int i = 0; i < hidden; i++) {
            KEEP.add(lookup.defineHiddenClass(payload, false).lookupClass());
        }
        System.out.println("hidden classes defined: " + KEEP.size());
        // Two warm-up rounds, then the timed ones.
        runRound(Math.min(yields, 2000));
        runRound(Math.min(yields, 2000));
        double[] perYield = new double[rounds];
        boolean all = true;
        for (int r = 0; r < rounds; r++) {
            long[] out = runRound(yields);
            all &= out[1] == yields;
            perYield[r] = out[0] / (double) yields;
        }
        Arrays.sort(perYield);
        System.err.printf("[gci-dk18] hidden=%d yields=%d rounds=%d ns_per_yield_median=%.1f min=%.1f max=%.1f%n",
                hidden, yields, rounds, perYield[rounds / 2], perYield[0], perYield[rounds - 1]);
        System.out.println("every yield completed: " + all);
        System.out.println("DONE");
    }

    /** One virtual thread yielding {@code n} times: {elapsed ns, yields done}. */
    static long[] runRound(int n) throws Exception {
        long[] out = new long[2];
        Thread vt = Thread.ofVirtual().start(() -> {
            long t0 = System.nanoTime();
            long done = 0;
            for (int i = 0; i < n; i++) {
                Thread.yield();
                done++;
            }
            out[0] = System.nanoTime() - t0;
            out[1] = done;
        });
        vt.join();
        acc += out[1];
        return out;
    }
}
