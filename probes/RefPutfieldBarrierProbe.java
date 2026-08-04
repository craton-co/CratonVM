// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * COV-03 — a JIT-compiled reference {@code putfield} must carry its write
 * barrier.
 *
 * <p>The optimizing (C2/IR) tier learned to compile a reference field STORE.
 * Its only correct lowering is {@code jit_putfield_object}, which performs the
 * SATB pre-barrier on the overwritten reference and the collector's own
 * post-write barrier — the remembered-set edge through which an OLD object's
 * field keeps a YOUNG object reachable. Emit the raw store without it and every
 * unit test still passes: the field holds the right bits, reads back the right
 * object, and nothing faults. The damage only appears one young collection
 * later, and it appears as a LOST OBJECT rather than as a crash at the store.
 *
 * <p>So that is what this probe stages, in this order:
 *
 * <ol>
 *   <li>allocate a fleet of {@code Holder}s and warm the accessors while
 *       churning allocation, so young collections run and the holders are
 *       promoted OLD;
 *   <li>store a FRESH young {@code Payload} into each old holder through the
 *       compiled setter, and drop every other reference to it — the old
 *       holder's field is now the only path to that object;
 *   <li>churn hard enough to force young collections;
 *   <li>read every payload back and check its contents.
 * </ol>
 *
 * <p>With the barrier, all payloads survive intact. Without it the collector
 * never scans the old holders, the payloads look unreachable, and step 4 finds
 * them missing or overwritten. A stale pointer may also fault — that is a
 * failure too, just a louder one.
 *
 * <p>Run it under the optimizing tier, or it proves nothing about the tier it
 * was written for:
 *
 * <pre>
 *   CRATONVM_JIT=force-c2 CRATONVM_DBG=ir-compiles \
 *     cratonvm -cp probes RefPutfieldBarrierProbe
 * </pre>
 *
 * and confirm in that log that {@code set}/{@code get} actually produced an
 * optimizing body. A run in which they fell back to the single-pass backend is
 * vacuous, not passing.
 */
public class RefPutfieldBarrierProbe {

    /** The old-generation object whose reference field is written. */
    static final class Holder {
        Object ref;
    }

    /**
     * The young object the store publishes. Several fields, checked on the way
     * out: a lost object can come back as null, as a different object, or as
     * the same address holding something else, and only a content check catches
     * the third.
     */
    static final class Payload {
        int a;
        int b;
        int c;
        int d;

        Payload(int seed) {
            a = seed;
            b = seed + 1;
            c = seed + 2;
            d = seed + 3;
        }

        boolean intact(int seed) {
            return a == seed && b == seed + 1 && c == seed + 2 && d == seed + 3;
        }
    }

    /** The method under test: `aload_0; aload_1; putfield; return`, nothing else. */
    static void set(Holder h, Object v) {
        h.ref = v;
    }

    /** Its read side: `aload_0; getfield; areturn`. */
    static Object get(Holder h) {
        return h.ref;
    }

    /**
     * Allocate {@code megabytes} of short-lived garbage. Kept out of line, and
     * the result folded into a sink, so it is not optimized away.
     *
     * <p>The VOLUME is the whole point. The first version of this probe churned
     * 76 MB against a 512 MB heap and ran to completion with
     * {@code [GC] generational: minor=0 major=0} — it passed without a single
     * collection, which is not a passing barrier test, it is a vacuous one.
     * Size the churn against the heap, and check the collection count in the
     * runner.
     */
    private static long churn(int megabytes) {
        long sink = 0;
        for (int mb = 0; mb < megabytes; mb++) {
            for (int i = 0; i < 1024; i++) {
                byte[] b = new byte[1024];
                b[0] = (byte) i;
                b[1023] = (byte) mb;
                sink += b[0] + b[1023];
            }
        }
        return sink;
    }

    public static void main(String[] args) {
        final int holders = Integer.getInteger("probe.holders", 4000);
        final int warm = Integer.getInteger("probe.warm", 300000);
        // Rounds of publish → collect → verify. Each round re-publishes fresh
        // young payloads into the same (long since old) holders, so the window
        // the barrier has to survive is entered repeatedly rather than once.
        final int rounds = Integer.getInteger("probe.rounds", 4);
        final int churnMb = Integer.getInteger("probe.churnmb", 192);

        Holder[] old = new Holder[holders];
        for (int i = 0; i < holders; i++) {
            old[i] = new Holder();
        }

        // 1. Warm `set`/`get` past the C2 threshold while allocating, so the
        //    collections that promote `old[]` happen BEFORE the payloads are
        //    published. A warm-up that allocates nothing leaves the holders
        //    young, and a young→young store needs no card — the probe would
        //    pass for the wrong reason.
        Object filler = new Object();
        long sink = 0;
        for (int i = 0; i < warm; i++) {
            Holder h = old[i % holders];
            set(h, filler);
            if (get(h) != filler) {
                throw new IllegalStateException("warm-up read-back at i=" + i);
            }
            if ((i & 0xfff) == 0) {
                sink += churn(1);
            }
        }
        sink += churn(churnMb);

        int missing = 0;
        int corrupt = 0;
        int wrongType = 0;
        for (int r = 0; r < rounds; r++) {
            // 2. Publish a fresh young payload into each (now old) holder
            //    through the compiled setter. After this loop the ONLY
            //    reference to each payload is the old holder's field, so the
            //    collector can reach it ONLY through the remembered-set / card
            //    edge the store's write barrier is responsible for recording.
            for (int i = 0; i < holders; i++) {
                set(old[i], new Payload(i + r));
            }

            // 3. Collect, hard.
            sink += churn(churnMb);

            // 4. Read them back.
            for (int i = 0; i < holders; i++) {
                Object o = get(old[i]);
                if (o == null) {
                    missing++;
                } else if (!(o instanceof Payload)) {
                    wrongType++;
                } else if (!((Payload) o).intact(i + r)) {
                    corrupt++;
                }
            }
        }

        // 5. A null receiver must still throw. The helper returns silently on
        //    an implausible receiver, so an unguarded call would turn a
        //    NullPointerException into a dropped store — which is exactly what
        //    a lost object looks like, from a different cause. The inline null
        //    check that deopts is what keeps the two apart.
        int npeFailures = expectNpe("set(null)", () -> set(null, filler))
                + expectNpe("get(null)", () -> get(null));

        int bad = missing + corrupt + wrongType + npeFailures;
        System.out.println("[refbarrier] holders=" + holders
                + " rounds=" + rounds
                + " checks=" + ((long) holders * rounds)
                + " missing=" + missing
                + " wrongType=" + wrongType
                + " corrupt=" + corrupt
                + " npeFailures=" + npeFailures
                + " sink=" + sink);
        System.out.println(bad == 0 ? "[refbarrier] PASS" : "[refbarrier] FAIL");
        if (bad != 0) {
            System.exit(1);
        }
    }

    private static int expectNpe(String what, Runnable r) {
        try {
            r.run();
        } catch (NullPointerException expected) {
            return 0;
        } catch (Throwable t) {
            System.out.println("[refbarrier] FAIL " + what + " threw " + t);
            return 1;
        }
        System.out.println("[refbarrier] FAIL " + what + " did not throw");
        return 1;
    }
}
