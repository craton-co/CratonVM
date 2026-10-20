// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t1/mk: a weak referent read through sun.misc.Unsafe (and so through
// jdk.internal.misc.Unsafe.getReference) while a ZGC concurrent mark cycle is
// open. Since dev 43e50d605c the cycle hides every active Weak referent slot
// from the marker (CRATONVM_ZGC_CONC_REF_SKIP, default ON); a read that hands
// the referent to the program must log it through the SATB barrier, as
// Reference.get() does. The Unsafe read does not
// (docs/internal/gc/zgc-zt1mk-referent-reads-without-a-keep-alive-under-concurrent-reference-skip-FIXED-20261019.md).
//
// Each round makes `per` payloads reachable only through a WeakReference,
// churns garbage (to open and close concurrent cycles), reads each referent
// with Unsafe.getObject(ref, referentOffset) and parks a non-null result in a
// long-lived holder array, churns again, and finally checks every parked
// payload: intact, and its WeakReference still naming it (a strongly
// reachable referent is never cleared). A lost keep-alive shows as bad > 0 or
// as a crash. HotSpot prints the same two lines.
//
// Every line on stdout is deterministic:
//   Zt1MkUnsafeReferent rounds=<r> per=<p> bad=0 cleared_while_held=0
//   Zt1MkUnsafeReferent after-gc bad=0 cleared_while_held=0
// Args: [rounds] [per] [churn-bytes-per-round] (default 200, 400, 4 MiB).
import java.lang.ref.Reference;
import java.lang.ref.WeakReference;
import java.lang.reflect.Field;

public class Zt1MkUnsafeReferent {
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

    static void churn(int bytes) {
        for (int done = 0; done < bytes; done += 1024) {
            sink = new byte[1024];
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        int churnBytes = args.length > 2 ? Integer.parseInt(args[2]) : 4 << 20;

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

        for (int r = 0; r < rounds; r++) {
            int base = r * per;
            for (int i = 0; i < per; i++) {
                refs[base + i] = new WeakReference<>(new Payload(base + i));
            }
            churn(churnBytes / 2);
            for (int i = 0; i < per; i++) {
                Object o = u.getObject(refs[base + i], referentOffset);
                if (o != null) {
                    holder[base + i] = o;
                }
            }
            churn(churnBytes / 2);
        }
        report("rounds=" + rounds + " per=" + per, holder, refs);
        System.gc();
        report("after-gc", holder, refs);
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
        System.out.println("Zt1MkUnsafeReferent " + tag + " bad=" + bad
                + " cleared_while_held=" + clearedWhileHeld);
    }
}
