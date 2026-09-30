// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * The rest of the `final`-class devirtualisation's blast radius, enumerated
 * and priced.
 *
 * `String` was the expensive case because its alternative is INLINE code with
 * no call at all — see the retired
 * `string-charat-loop-cost-and-the-unsteerable-intrinsic-FIXED-20260902`
 * write-up. That page left "any other final class with an instance call-site
 * intrinsic ... has not been enumerated" open. The enumeration is three
 * classes — `String`, `Integer`, `Long` — and the other two are what this
 * probe measures.
 *
 * `Integer.intValue()I` and `Long.longValue()J` are served by a THIN NATIVE
 * HELPER bind in the single-pass invoke ladder, not by an inline expansion, so
 * whichever way the classification goes the site pays a CALL. This probe asks
 * whether the two calls cost differently:
 *
 *   default                          — the devirtualisation claims the site and
 *                                      binds the compiled `Integer.intValue`
 *                                      body (a Java frame, shadow stack push,
 *                                      invocation counter)
 *   CRATONVM_JIT_FINAL_DEVIRT=0      — the site stays virtual and the ladder's
 *                                      thin `integer_int_value_direct` helper
 *                                      bind takes it (a Rust call, no frame)
 *
 * A flat pair of columns says the yield `String` needed buys nothing here and
 * the set that mattered really was `{String}`.
 *
 *   java     -cp probes FinalDevirtInstanceIntrinsics
 *   cratonvm -cp probes FinalDevirtInstanceIntrinsics
 *   CRATONVM_JIT_FINAL_DEVIRT=0 cratonvm -cp probes FinalDevirtInstanceIntrinsics
 */
public class FinalDevirtInstanceIntrinsics {

    static int sumInts(Integer[] xs, int reps) {
        int acc = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < xs.length; i++)
                acc += xs[i].intValue();
        return acc;
    }

    static long sumLongs(Long[] xs, int reps) {
        long acc = 0;
        for (int r = 0; r < reps; r++)
            for (int i = 0; i < xs.length; i++)
                acc += xs[i].longValue();
        return acc;
    }

    static void row(String n, int reps, int n_elems, long ns, long g) {
        long ops = (long) reps * (long) n_elems;
        System.out.printf("%-10s reps=%-5d %8.2f ns/op  %6d ms  g=%d%n",
                n, reps, (double) ns / ops, ns / 1_000_000, g);
    }

    public static void main(String[] args) {
        final int N = 100_000;
        Integer[] ints = new Integer[N];
        Long[] longs = new Long[N];
        for (int i = 0; i < N; i++) {
            // Past the box cache on purpose: a cached box would make every
            // element the same object and let a receiver profile collapse the
            // site into something this probe is not about.
            ints[i] = Integer.valueOf(1000 + i);
            longs[i] = Long.valueOf(1000L + i);
        }
        for (int reps : new int[]{2, 20, 200, 200}) {
            long t = System.nanoTime();
            int g = sumInts(ints, reps);
            row("intValue", reps, N, System.nanoTime() - t, g);
        }
        for (int reps : new int[]{2, 20, 200, 200}) {
            long t = System.nanoTime();
            long g = sumLongs(longs, reps);
            row("longValue", reps, N, System.nanoTime() - t, g);
        }
    }
}
