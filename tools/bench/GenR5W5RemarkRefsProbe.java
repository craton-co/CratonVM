// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * gen r5w5/conc9 (2026-09-27): remark-time reference processing of the
 * generational CONCURRENT cycle, on a workload that really opens cycles.
 *
 * <p>Pages: {@code docs/internal/gc/gengc-mark2-gen-concurrent-cycle-has-no-remark-reference-processing-FIXED-20260929.md}
 * and {@code gengc-r4w5-concmark5-remark-reference-processing-design-DONE-20260929.md}.
 * {@code GenR5W1RemarkRefProcProbe} was meant to exercise the hook, but its
 * retained ring died young at {@code -Xmx256m} (the old generation never
 * reached the concurrent start), so the hook never ran on it. This probe drives
 * the old generation the way {@code GenR5W3ConcUnloadProbe} does: every round
 * replaces one {@value #BALLAST_MIB} MiB {@code long[]} (humongous on
 * CratonVM, so it is placed in the old generation; tenured on HotSpot Serial)
 * and churns {@value #CHURN_MIB} MiB of young garbage. From round 1 the old
 * generation is past the 45 % concurrent start and below the 75 % STW floor,
 * so the collector that meets the dead referents is a concurrent cycle; on
 * HotSpot it is the full collection the tenured garbage forces.
 *
 * <p>Everything below is created, then PROMOTED while strongly held (churn,
 * no {@code System.gc()}), then dropped, so every {@code Reference} and every
 * referent is OLD when the cycles run:
 * <ul>
 *   <li>{@value #N} payloads with a {@code WeakReference} each; the even ones
 *       stay strongly held for the whole run, the odd ones become weakly
 *       reachable only;</li>
 *   <li>{@value #SOFT} payloads held only by {@code SoftReference}s (recently
 *       created; HotSpot's LRU policy keeps them);</li>
 *   <li>{@value #PHANTOM} payloads held only by {@code PhantomReference}s on a
 *       queue;</li>
 *   <li>one finalizable {@code Fin} holding a payload R, with a weak reference
 *       to {@code Fin}, a weak reference to R and a phantom reference to R.
 *       {@code Fin.finalize()} resurrects R into a static. HotSpot clears both
 *       weak references BEFORE finalization, and never enqueues the phantom
 *       (R is strongly reachable again once finalized).</li>
 * </ul>
 * During the first {@value #RACE_ROUNDS} rounds a second thread walks the weak
 * references WHILE cycles open, verifies every payload it gets, and rescues
 * every 97th odd payload it reads into a strong list (the
 * {@code Reference.get()} keep-alive race). The rounds then continue until
 * everything above has been processed, or {@code maxRounds} runs out.
 *
 * <p>Deterministic stdout ({@code java -XX:+UseSerialGC -Xmx256m} prints it):
 * <pre>
 *   remark-refs strong-cleared=0 corrupt=0 rescued-ok=true dead-weak-cleared=true soft-kept=64 phantoms-enqueued=64 finalized=1 resurrected-ok=true weak-to-finalizable-cleared=true weak-to-resurrected-cleared=true phantom-to-resurrected-enqueued=false
 * </pre>
 * Stderr (not compared): {@code [probe] rounds=R old_collections=N} where N is
 * the old-generation collector bean's count at the end. On CratonVM with
 * {@code CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1} the evidence that the
 * CONCURRENT remark did the work is {@code N=0} and, under
 * {@code CRATONVM_DBG=gc-stats}, {@code concdrv_remark_refproc_hook_calls>=1}
 * and {@code concdrv_remark_refproc_retired>=1}.
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W5RemarkRefsProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W5RemarkRefsProbe
 *   CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
 *     cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5RemarkRefsProbe
 * </pre>
 * Without the hook the concurrent cycle processes no reference, so the arm
 * prints {@code dead-weak-cleared=false} (and the phantom / weak-to-resurrected
 * fields that need a clearing collection) unless a STW major happens to run.
 * Usage: {@code GenR5W5RemarkRefsProbe [maxRounds]} (default 64). The sizes are
 * for {@code -Xmx256m}.
 */
public final class GenR5W5RemarkRefsProbe {

    static final int BALLAST_MIB = 34;
    static final int CHURN_MIB = 80;
    static final int N = 4096;
    static final int SOFT = 64;
    static final int PHANTOM = 64;
    static final int RACE_ROUNDS = 6;
    /** Rounds run after everything was processed, before the phantom check. */
    static final int SETTLE_ROUNDS = 3;

    static final class Payload {
        final int id;
        final long check;
        final long[] body = new long[4];

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

    /** Finalizable holder of R; resurrects R when finalized. */
    static final class Fin {
        final Payload child;

        Fin(Payload child) {
            this.child = child;
        }

        @SuppressWarnings({"removal", "deprecation"})
        @Override
        protected void finalize() {
            resurrected = child;
            FINALIZED.incrementAndGet();
        }
    }

    static final AtomicInteger FINALIZED = new AtomicInteger();
    static volatile Payload resurrected;

    // Held for the whole run.
    static Payload[] strong;
    static final List<WeakReference<Payload>> WEAK = new ArrayList<>();
    static final List<SoftReference<Payload>> SOFTS = new ArrayList<>();
    static final List<PhantomReference<Payload>> PHANTOMS = new ArrayList<>();
    static final ReferenceQueue<Payload> PHANTOM_QUEUE = new ReferenceQueue<>();
    static final ReferenceQueue<Payload> CHILD_QUEUE = new ReferenceQueue<>();
    static WeakReference<Fin> weakToFin;
    static WeakReference<Payload> weakToChild;
    static PhantomReference<Payload> phantomToChild;

    // Strong holders during the promotion phase only.
    static Object[] setupHolder;

    static volatile Object sink;
    static volatile long[] ballast;

    public static void main(String[] args) throws Exception {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        setup();
        promote();
        // The odd payloads, the soft and phantom referents, Fin and R: from
        // here on nothing strong names them.
        setupHolder = null;

        Racer racer = new Racer();
        Thread t = new Thread(racer, "remark-refs-racer");
        t.start();
        int phantoms = 0;
        int rounds = 0;
        for (; rounds < RACE_ROUNDS; rounds++) {
            round();
            phantoms += drainPhantoms();
        }
        racer.stop = true;
        t.join();

        int doneAt = -1;
        for (; rounds < maxRounds; rounds++) {
            round();
            phantoms += drainPhantoms();
            if (doneAt < 0 && processed(racer, phantoms)) {
                doneAt = rounds;
            }
            if (doneAt >= 0 && rounds - doneAt >= SETTLE_ROUNDS) {
                break;
            }
        }
        sink = null;
        ballast = null;

        int softKept = 0;
        for (SoftReference<Payload> s : SOFTS) {
            Payload p = s.get();
            if (p != null && p.ok()) {
                softKept++;
            }
        }
        Payload r = resurrected;
        boolean phantomToResurrected = CHILD_QUEUE.poll() != null;
        System.out.println("remark-refs strong-cleared=" + racer.strongCleared
                + " corrupt=" + racer.corrupt
                + " rescued-ok=" + racer.rescuedOk()
                + " dead-weak-cleared=" + racer.deadCleared()
                + " soft-kept=" + softKept
                + " phantoms-enqueued=" + phantoms
                + " finalized=" + FINALIZED.get()
                + " resurrected-ok=" + (r != null && r.ok())
                + " weak-to-finalizable-cleared=" + weakToFin.refersTo(null)
                + " weak-to-resurrected-cleared=" + weakToChild.refersTo(null)
                + " phantom-to-resurrected-enqueued=" + phantomToResurrected);
        System.err.println("[probe] rounds=" + rounds + " old_collections=" + oldCollections());
        // Keep the strong set and the rescued payloads reachable to the end.
        sink = strong;
        sink = racer.rescued;
    }

    /** One round: a new ballast (the previous one becomes old garbage), then
     *  young churn (at least one young collection). */
    static void round() {
        ballast = new long[(BALLAST_MIB << 20) / 8];
        final int churnObjects = CHURN_MIB << 10; // long[126] = 1 KiB each
        for (int k = 0; k < churnObjects; k++) {
            sink = new long[126];
        }
    }

    /** Everything is created in this frame, which never outlives it. */
    static void setup() {
        strong = new Payload[N];
        Payload[] odd = new Payload[N];
        for (int i = 0; i < N; i++) {
            Payload p = new Payload(i);
            WEAK.add(new WeakReference<>(p));
            if ((i & 1) == 0) {
                strong[i] = p;
            } else {
                odd[i] = p;
            }
        }
        Payload[] softReferents = new Payload[SOFT];
        for (int i = 0; i < SOFT; i++) {
            softReferents[i] = new Payload(N + i);
            SOFTS.add(new SoftReference<>(softReferents[i]));
        }
        Payload[] phantomReferents = new Payload[PHANTOM];
        for (int i = 0; i < PHANTOM; i++) {
            phantomReferents[i] = new Payload(2 * N + i);
            PHANTOMS.add(new PhantomReference<>(phantomReferents[i], PHANTOM_QUEUE));
        }
        Payload child = new Payload(3 * N);
        Fin fin = new Fin(child);
        weakToFin = new WeakReference<>(fin);
        weakToChild = new WeakReference<>(child);
        phantomToChild = new PhantomReference<>(child, CHILD_QUEUE);
        setupHolder = new Object[] {odd, softReferents, phantomReferents, fin};
    }

    /** Churn young garbage while everything is held, so all of it is
     *  PROMOTED (CratonVM tenures on the third survival; 16 x 32 MiB is eight
     *  64 MiB semi-spaces). Touches none of it. */
    static void promote() {
        for (int pass = 0; pass < 16; pass++) {
            for (int k = 0; k < 65536; k++) {
                sink = new long[64];
            }
        }
        sink = null;
    }

    static int drainPhantoms() {
        int n = 0;
        while (PHANTOM_QUEUE.poll() != null) {
            n++;
        }
        return n;
    }

    static boolean processed(Racer racer, int phantoms) {
        return racer.deadCleared()
                && phantoms == PHANTOM
                && FINALIZED.get() == 1
                && weakToFin.refersTo(null)
                && weakToChild.refersTo(null);
    }

    /** Walks the weak references while cycles open (its own thread, which
     *  ends before the final checks, so no stale local of it survives). */
    static final class Racer implements Runnable {
        volatile boolean stop;
        final List<Payload> rescued = new ArrayList<>();
        final boolean[] isRescued = new boolean[N];
        int strongCleared;
        int corrupt;
        int oddReads;

        @Override
        public void run() {
            while (!stop) {
                walk();
            }
        }

        void walk() {
            for (int i = 0; i < N; i++) {
                Payload p = WEAK.get(i).get();
                if ((i & 1) == 0) {
                    if (p == null) {
                        strongCleared++;
                    } else if (p != strong[i] || !p.ok()) {
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

        boolean rescuedOk() {
            boolean ok = true;
            for (Payload p : rescued) {
                ok &= p.ok() && WEAK.get(p.id).get() == p;
            }
            return ok;
        }

        /** Every odd payload the racer did not rescue has been cleared.
         *  {@code refersTo}, not {@code get()}: a {@code get()} during an open
         *  cycle is a keep-alive (SATB) that would itself keep the referent
         *  for that cycle. */
        boolean deadCleared() {
            for (int i = 1; i < N; i += 2) {
                if (!isRescued[i] && !WEAK.get(i).refersTo(null)) {
                    return false;
                }
            }
            return true;
        }
    }

    /** The old-generation collector bean's count ("MarkSweepCompact" on the
     *  Serial-shaped beans both VMs expose), or -1 if there is none. */
    static long oldCollections() {
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            String n = gc.getName();
            if (n.contains("MarkSweep") || n.contains("Old")) {
                return gc.getCollectionCount();
            }
        }
        return -1;
    }
}
