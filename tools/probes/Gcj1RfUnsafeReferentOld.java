// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gcj j1/rf: the Generational twin of Zt1MkUnsafeReferent. A weak referent is
// read through sun.misc.Unsafe (jdk.internal.misc.Unsafe.getReference) while a
// Generational concurrent old-generation cycle is open. That cycle hides slot 0
// of every OLD java.lang.ref.Reference it was handed at initial mark
// (CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK, default ON), so a read that hands the
// referent to the program must log it through the SATB barrier, as
// Reference.get() does; HotSpot's Unsafe_GetReference does
// (ON_UNKNOWN_OOP_REF). Without it (CRATONVM_GEN_REFERENT_READ_KEEP_ALIVE=0) a
// referent parked in an already-scanned old array is reachable only through an
// edge the snapshot never saw: the remark clears its WeakReference and the sweep
// frees it while it is strongly held.
//
// Unlike the ZGC probe, the References and their payloads must be OLD for the
// hiding to apply: each round holds its payloads strongly while young churn
// tenures them (run with -XX:MaxTenuringThreshold=1), then drops the strong
// path, keeps old-generation garbage flowing (a ring of 64 KiB arrays) so
// concurrent cycles keep opening, and reads every referent with
// Unsafe.getObject, parking a non-null result in a long-lived holder that was
// allocated first (an early-scanned old object).
//
// Every line on stdout is deterministic (HotSpot prints the same two lines):
//   Gcj1RfUnsafeReferentOld rounds=<r> per=<p> bad=0 cleared_while_held=0
//   Gcj1RfUnsafeReferentOld after-gc bad=0 cleared_while_held=0
// stderr: parked=<n> cpu_ms=<t> (not compared).
// Args: [rounds] [per] [young-churn-bytes-per-round] (default 40, 2000, 16 MiB).
import java.lang.management.ManagementFactory;
import java.lang.ref.Reference;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;

public class Gcj1RfUnsafeReferentOld {
    static final class Payload {
        final int id;
        final int check;
        final int[] data;

        Payload(int id) {
            this.id = id;
            this.check = id * 31 + 7;
            this.data = new int[8];
            for (int k = 0; k < 8; k++) {
                data[k] = id + k;
            }
        }

        boolean intact(int i) {
            if (id != i || check != i * 31 + 7 || data == null || data.length != 8) {
                return false;
            }
            for (int k = 0; k < 8; k++) {
                if (data[k] != i + k) {
                    return false;
                }
            }
            return true;
        }
    }

    static volatile Object sink;
    static Object[] ballast;
    static int ballastNext;

    static void churn(int bytes) {
        for (int done = 0; done < bytes; done += 16384) {
            sink = new byte[16384];
        }
    }

    /** Replaces `count` slots of the old-generation ballast ring (64 KiB each). */
    static void oldGarbage(int count) {
        for (int k = 0; k < count; k++) {
            ballast[ballastNext] = new long[8192];
            ballastNext = (ballastNext + 1) % ballast.length;
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        int churnBytes = args.length > 2 ? Integer.parseInt(args[2]) : 16 << 20;
        long cpu0 = cpuNanos();

        Field theUnsafe = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        theUnsafe.setAccessible(true);
        sun.misc.Unsafe u = (sun.misc.Unsafe) theUnsafe.get(null);
        long referentOffset = u.objectFieldOffset(Reference.class.getDeclaredField("referent"));

        int n = rounds * per;
        // Allocated first, so a cycle scans it early: a referent parked in a
        // slot the marker already passed is reachable only through the parking
        // store, whose pre-barrier logs the OLD value (null).
        Object[] holder = new Object[n];
        @SuppressWarnings("unchecked")
        WeakReference<Payload>[] refs = new WeakReference[n];
        ballast = new Object[256]; // 16 MiB of 64 KiB arrays at most

        int parked = 0;
        for (int r = 0; r < rounds; r++) {
            int base = r * per;
            Payload[] strong = new Payload[per];
            for (int i = 0; i < per; i++) {
                strong[i] = new Payload(base + i);
                refs[base + i] = new WeakReference<>(strong[i]);
            }
            // Tenure the payloads and their WeakReferences while strongly held.
            for (int pass = 0; pass < 3; pass++) {
                churn(churnBytes / 4);
                oldGarbage(8);
            }
            strong = null;
            churn(churnBytes / 8);
            oldGarbage(16);
            for (int i = 0; i < per; i++) {
                Object o = u.getObject(refs[base + i], referentOffset);
                if (o != null) {
                    holder[base + i] = o;
                    parked++;
                }
                if ((i & 63) == 0) {
                    oldGarbage(1);
                    churn(16384 * 4);
                }
            }
            churn(churnBytes / 8);
        }
        report("rounds=" + rounds + " per=" + per, holder, refs);
        System.gc();
        report("after-gc", holder, refs);
        System.err.println("Gcj1RfUnsafeReferentOld parked=" + parked
                + " cpu_ms=" + (cpuNanos() - cpu0) / 1_000_000);
    }

    static long cpuNanos() {
        try {
            return ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        } catch (Throwable t) {
            return System.nanoTime();
        }
    }

    static void report(String tag, Object[] holder, WeakReference<Payload>[] refs) {
        int bad = 0;
        int clearedWhileHeld = 0;
        for (int i = 0; i < holder.length; i++) {
            Object o = holder[i];
            if (o == null) {
                continue;
            }
            if (!(o instanceof Payload) || !((Payload) o).intact(i)) {
                bad++;
                continue;
            }
            if (refs[i].get() != o) {
                clearedWhileHeld++;
            }
        }
        System.out.println("Gcj1RfUnsafeReferentOld " + tag + " bad=" + bad
                + " cleared_while_held=" + clearedWhileHeld);
    }
}
