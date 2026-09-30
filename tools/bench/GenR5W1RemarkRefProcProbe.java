// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

/**
 * gen r5w1/refs5 (2026-09-26): remark-time reference processing in the
 * generational CONCURRENT cycle — correctness fence and evidence.
 *
 * <p>With {@code CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1} the concurrent
 * cycle hides the referent slot of every old-gen {@code Reference} from its
 * trace and clears or keeps each referent at remark. That is only correct if
 * (a) a weak reference whose referent is strongly reachable is never cleared,
 * (b) a referent the program obtains through {@code get()} and stores
 * strongly DURING the cycle survives it (the {@code Reference.get()}
 * keep-alive barrier), and (c) nothing a live reference names is freed. This
 * program checks all three while keeping the old generation busy enough for
 * concurrent cycles to run:
 * <ul>
 *   <li>{@code n} payloads, each with a weak reference; the even ones are also
 *       held strongly, the odd ones only weakly;</li>
 *   <li>every round it walks the weak references: a strongly held payload must
 *       still be there and intact; an odd one that is still there is verified,
 *       and every 97th such read is RESCUED into a strong list (the get-and-
 *       publish race);</li>
 *   <li>between walks it promotes a ring of retained blocks and churns young
 *       garbage, so the old generation crosses the concurrent start and cycles
 *       open and remark while the walks run.</li>
 * </ul>
 * Every payload carries a checksum of its id; a payload freed while reachable
 * reads back wrong (or crashes). After the rounds it runs {@code System.gc()}
 * twice and checks that every odd payload not rescued has been cleared.
 *
 * <p>Deterministic output (HotSpot prints the same):
 * <pre>
 *   remark-refproc n=20000 rounds=40 strong-cleared=0 corrupt=0 rescued-ok=true after-gc-dead-cleared=true
 * </pre>
 * The evidence that the CONCURRENT cycle, not a full collection, cleared the
 * old-gen weak references is on stderr under {@code CRATONVM_DBG=gc-stats}:
 * {@code [GC] conc_driver: ... concdrv_remark_refproc_hook_calls=H ...
 * concdrv_reference_skip_published=S ... concdrv_remark_refproc_retired=R}
 * with H, S, R &gt; 0.
 * <pre>
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W1RemarkRefProcProbe
 *   CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_GEN_CONC_SERVICE_THREAD=1 CRATONVM_DBG=gc-stats \
 *     cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W1RemarkRefProcProbe
 * </pre>
 * Usage: GenR5W1RemarkRefProcProbe [n] [rounds].
 */
public final class GenR5W1RemarkRefProcProbe {
    static final class Payload {
        final int id;
        final long check;
        final long[] body = new long[6];

        Payload(int id) {
            this.id = id;
            this.check = mix(id);
            for (int i = 0; i < body.length; i++) {
                body[i] = check ^ i;
            }
        }

        boolean ok() {
            if (check != mix(id)) {
                return false;
            }
            for (int i = 0; i < body.length; i++) {
                if (body[i] != (check ^ i)) {
                    return false;
                }
            }
            return true;
        }
    }

    static long mix(int id) {
        long x = id * 0x9E3779B97F4A7C15L;
        return x ^ (x >>> 29);
    }

    static volatile Object sink;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20_000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        List<WeakReference<Payload>> weak = new ArrayList<>(n);
        Payload[] strong = build(n, weak);
        Walk w = new Walk(n);
        Object[] ring = new Object[512];
        for (int r = 0; r < rounds; r++) {
            // In its own frame, so no dead local of this one can keep the last
            // payload read alive into the final System.gc() check below.
            w.walk(weak, strong);
            // Promote: a ring of 512 x 64 KiB retained blocks turns over every
            // round, so the old generation keeps filling and emptying.
            for (int k = 0; k < 64; k++) {
                ring[(r * 64 + k) & 511] = new long[8 * 1024];
            }
            // Young churn.
            for (int k = 0; k < 4096; k++) {
                sink = new long[32];
            }
        }
        boolean rescuedOk = w.rescuedOk(weak);
        ring = null;
        System.gc();
        System.gc();
        boolean deadCleared = w.deadCleared(weak);
        System.out.println("remark-refproc n=" + n + " rounds=" + rounds
                + " strong-cleared=" + w.strongCleared + " corrupt=" + w.corrupt
                + " rescued-ok=" + rescuedOk + " after-gc-dead-cleared=" + deadCleared);
        // Keep the strong set reachable to the end.
        sink = strong;
    }

    /** The payloads (in their own frame, for the same reason as the walk). */
    static Payload[] build(int n, List<WeakReference<Payload>> weak) {
        Payload[] strong = new Payload[n];
        for (int i = 0; i < n; i++) {
            Payload p = new Payload(i);
            weak.add(new WeakReference<>(p));
            if ((i & 1) == 0) {
                strong[i] = p;
            }
        }
        return strong;
    }

    /** The per-round walk and its tallies. */
    static final class Walk {
        final List<Payload> rescued = new ArrayList<>();
        final boolean[] isRescued;
        int strongCleared;
        int corrupt;
        int oddReads;

        Walk(int n) {
            isRescued = new boolean[n];
        }

        void walk(List<WeakReference<Payload>> weak, Payload[] strong) {
            for (int i = 0; i < strong.length; i++) {
                Payload p = weak.get(i).get();
                if ((i & 1) == 0) {
                    if (p == null) {
                        strongCleared++;
                    } else if (!p.ok() || p != strong[i]) {
                        corrupt++;
                    }
                } else if (p != null) {
                    if (!p.ok()) {
                        corrupt++;
                    } else if (!isRescued[i] && (++oddReads % 97) == 0) {
                        rescued.add(p);
                        isRescued[i] = true;
                    }
                }
            }
        }

        boolean rescuedOk(List<WeakReference<Payload>> weak) {
            boolean ok = true;
            for (Payload p : rescued) {
                ok &= p.ok() && weak.get(p.id).get() == p;
            }
            return ok;
        }

        boolean deadCleared(List<WeakReference<Payload>> weak) {
            for (int i = 1; i < isRescued.length; i += 2) {
                if (!isRescued[i] && weak.get(i).get() != null) {
                    return false;
                }
            }
            return true;
        }
    }
}
