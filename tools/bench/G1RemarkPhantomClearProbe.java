// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.PhantomReference;
import java.lang.ref.Reference;
import java.lang.ref.ReferenceQueue;

/**
 * G1's final remark enqueues a phantom reference to a dead OLD object; JDK 9+
 * clears a phantom reference as it enqueues it.
 *
 * <p>Page: {@code docs/internal/gc/gengc-r5w1-refs5-g1-remark-leaves-enqueued-phantom-referent-dangling-FIXED-20261005.md}.
 * The remark is the one G1 point that sees an old referent die, and it linked
 * the phantom into its queue without nulling the referent slot; {@code cleanup}
 * then freed the referent's region in place, so the live
 * {@code PhantomReference} named freed memory. Java sees it through
 * {@link Reference#refersTo}: an enqueued phantom must answer
 * {@code refersTo(null) == true}.
 *
 * <ol>
 *   <li>allocates {@value #REFS} small referents, each with a phantom
 *       reference registered with one queue (the references stay strongly
 *       held, as a {@code Cleaner}'s list holds its own), and tenures them
 *       with {@value #TENURE_GCS} {@code System.gc()} calls (on CratonVM's G1
 *       a young pause and then a whole mark / remark / cleanup cycle);</li>
 *   <li>drops the referents, keeps a {@value #BALLAST_MIB} MiB ballast live,
 *       and churns young garbage with a {@code System.gc()} every fourth
 *       round, polling the queue, until every reference was enqueued or
 *       {@code maxRounds} rounds passed. The referents are old by then, so
 *       only a remark can see them die;</li>
 *   <li>counts the dequeued references that still do NOT refer to null.</li>
 * </ol>
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseG1GC -Xmx256m
 * -XX:MaxTenuringThreshold=1 -XX:InitiatingHeapOccupancyPercent=35} prints the same):
 * <pre>
 *   g1-remark-phantom all-enqueued=true not-cleared=0
 * </pre>
 * Before the fix CratonVM printed {@code not-cleared=} the number of phantoms
 * the remark (rather than a young pause, whose pre-collection pass nulls them)
 * enqueued.
 * <pre>
 *   javac -d tools/bench tools/bench/G1RemarkPhantomClearProbe.java
 *   java -XX:+UseG1GC -Xmx256m -XX:MaxTenuringThreshold=1 -XX:InitiatingHeapOccupancyPercent=35 -cp tools/bench G1RemarkPhantomClearProbe
 *   cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx256m -XX:MaxTenuringThreshold=1 -XX:InitiatingHeapOccupancyPercent=35 -cp tools/bench G1RemarkPhantomClearProbe
 * </pre>
 * Usage: {@code G1RemarkPhantomClearProbe [maxRounds]} (default 200).
 */
public final class G1RemarkPhantomClearProbe {

    static final int REFS = 100_000;
    static final int BALLAST_MIB = 110;
    static final int CHURN_MIB = 32;
    static final int TENURE_GCS = 3;

    static final class Referent {
        long a;
        Object b;
        Referent(long a) {
            this.a = a;
        }
    }

    static volatile Object sink;

    static void churn(int mib) {
        for (int i = 0; i < mib * 16; i++) {
            sink = new byte[64 * 1024 - 16];
        }
    }

    public static void main(String[] args) {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        ReferenceQueue<Referent> queue = new ReferenceQueue<>();
        @SuppressWarnings("unchecked")
        PhantomReference<Referent>[] refs = new PhantomReference[REFS];
        Referent[] referents = new Referent[REFS];
        for (int i = 0; i < REFS; i++) {
            referents[i] = new Referent(i);
            referents[i].b = referents[(i * 7) % (i + 1)];
            refs[i] = new PhantomReference<>(referents[i], queue);
        }
        // Tenure the referents (and the references): on G1 a System.gc()
        // is a young pause and then a complete mark / remark / cleanup cycle.
        for (int r = 0; r < TENURE_GCS; r++) {
            churn(CHURN_MIB);
            System.gc();
        }
        byte[][] ballast = new byte[BALLAST_MIB][];
        for (int i = 0; i < BALLAST_MIB; i++) {
            ballast[i] = new byte[1024 * 1024 - 64];
        }
        referents = null;

        int enqueued = 0;
        int notCleared = 0;
        for (int round = 0; round < maxRounds && enqueued < REFS; round++) {
            churn(CHURN_MIB);
            if (round % 4 == 0) {
                // The cycle whose remark sees the old referents die.
                System.gc();
            }
            Reference<? extends Referent> ref;
            while ((ref = queue.poll()) != null) {
                enqueued++;
                if (!ref.refersTo(null)) {
                    notCleared++;
                }
            }
        }
        sink = ballast;
        System.out.println("g1-remark-phantom all-enqueued=" + (enqueued == REFS)
                + " not-cleared=" + notCleared);
        if (refs.length != REFS) {
            throw new AssertionError();
        }
    }
}
