// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;

/**
 * gen r5w2/conc6 (2026-09-26): a SoftReference created since the last
 * collection survives the next one however long ago it was created — HotSpot's
 * {@code LRUMaxHeapPolicy}, measured where CratonVM's default policy cannot
 * hide the difference.
 *
 * <p><b>Why {@code GenR5W1SoftRefYoungProbe} could not show it.</b> HotSpot
 * compares a reference's timestamp with the soft CLOCK, which is the END of the
 * previous collection; a reference created (or read) after that collection has
 * idle time 0 at the next one and is never cleared by the policy. CratonVM's
 * default pre-collection pass compares the wall clock NOW with the creation
 * stamp, against {@code min(young free, old free)} measured BEFORE the
 * collection, at {@code SoftRefLRUPolicyMSPerMB} (1000) ms per MB. The wave-1
 * probe's references were a few milliseconds old at their collection, and the
 * young trigger fires with 50 % (moving young) or 10 % (non-moving young) of
 * the young semi-space still free — at {@code -Xmx256m} that is 32 MB or 6 MB,
 * i.e. a 32 s or 6 s idle allowance. Nothing a few milliseconds old is ever
 * condemned there, which is what the orchestrator measured (HotSpot's lines on
 * the default arm too).
 *
 * <p><b>This probe</b> makes the reference older than any allowance the default
 * can compute: the young semi-space is a quarter of {@code -Xmx}, so the
 * default's free-space figure is at most {@code maxMemory/4} MB and its idle
 * allowance at most that many seconds. Each round creates a soft reference to
 * a fresh 256-byte array (young), does not read it, sleeps
 * {@code 1000 * maxMemory/4/MB + 1500} ms, then allocates 16 KiB arrays until a
 * {@link WeakReference} canary created after the sleep has been cleared (a
 * collection ran), and only then reads the soft reference.
 *
 * <ul>
 *   <li>HotSpot (any collector): no collection ran between creation and the
 *       canary's collection, so the idle time is 0 and nothing is lost.</li>
 *   <li>CratonVM with {@code CRATONVM_SOFTREF_HOTSPOT_LRU=1}: the clock is the
 *       previous processing round's end, which precedes the creation, so the
 *       idle time is 0 and nothing is lost.</li>
 *   <li>CratonVM default: the idle time (the sleep) exceeds the allowance, the
 *       pre-collection pass condemns the reference, and the young referent is
 *       freed: {@code lost=rounds}.</li>
 * </ul>
 *
 * <p>Deterministic output (the sleep length is not printed):
 * <pre>
 *   soft-idle rounds=2 collected=2 lost=0
 *   PASS
 * </pre>
 * {@code collected} counts rounds whose canary was cleared within the
 * allocation cap (1 GiB); a round that never collected cannot judge the policy
 * and makes the verdict {@code INCONCLUSIVE}.
 *
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W2SoftRefIdleYoungProbe.java
 *   java -XX:+UseSerialGC -Xmx32m -cp tools/bench GenR5W2SoftRefIdleYoungProbe
 *   cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx32m -cp tools/bench GenR5W2SoftRefIdleYoungProbe
 *   CRATONVM_SOFTREF_HOTSPOT_LRU=1 cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx32m -cp tools/bench GenR5W2SoftRefIdleYoungProbe
 * </pre>
 * Expected: HotSpot and the flag arm print the two lines above; the CratonVM
 * default arm prints {@code soft-idle rounds=2 collected=2 lost=2} and
 * {@code FAIL} (exit status 1). Run time about 2 x 9.5 s at {@code -Xmx32m}.
 *
 * <p>{@code -XX:SoftRefLRUPolicyMSPerMB=0} is a second oracle: HotSpot keeps
 * the references anyway (idle time 0 is not greater than an interval of 0), so
 * it prints {@code PASS} too, and CratonVM's default arm still loses them.
 *
 * <p>Usage: {@code GenR5W2SoftRefIdleYoungProbe [rounds] [idleMs]} (defaults 2
 * and the formula above).
 */
public final class GenR5W2SoftRefIdleYoungProbe {
    static volatile Object sink;

    public static void main(String[] args) throws InterruptedException {
        final long mb = 1024L * 1024L;
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 2;
        long idleMs = args.length > 1
                ? Long.parseLong(args[1])
                : 1000L * (Runtime.getRuntime().maxMemory() / 4 / mb) + 1500L;
        int lost = 0;
        int collected = 0;
        for (int r = 0; r < rounds; r++) {
            SoftReference<byte[]> soft = new SoftReference<>(new byte[256]);
            Thread.sleep(idleMs);
            WeakReference<Object> canary = new WeakReference<>(new Object());
            int i = 0;
            while (canary.get() != null && i < 65536) {
                sink = new byte[16 * 1024];
                i++;
            }
            if (canary.get() == null) {
                collected++;
            }
            if (soft.get() == null) {
                lost++;
            }
        }
        System.out.println("soft-idle rounds=" + rounds + " collected=" + collected + " lost=" + lost);
        if (lost != 0) {
            System.out.println("FAIL");
            System.exit(1);
        }
        System.out.println(collected == rounds ? "PASS" : "INCONCLUSIVE");
    }
}
