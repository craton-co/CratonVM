// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// zt t3/gn (CRATONVM_ZGC_GEN_FULL_REQUEST_MAJOR): System.gc() on a
// GENERATIONAL ZGC heap must run a major (whole-heap) collection, as HotSpot
// 25's generational ZGC does (`Major Collection (System.gc())`). Before the
// fix the full request never reached the cycle-kind decision, so once the
// heap had an old generation a System.gc() ran a YOUNG cycle: an OLD object
// that had died survived it, and its WeakReference still answered.
//
// Each round makes `per` referents that are only weakly reachable through an
// Object[] holder, ages them into the old generation with `age` System.gc()
// calls (the default promotion age is 3; HotSpot's System.gc() promotes every
// survivor at once), drops the holder, churns a little garbage so the next
// collection is not the one right after the drop, calls System.gc() once and
// counts the WeakReferences that still answer.
//
// Every line on stdout is deterministic:
//   Zt3GnSystemGcMajor rounds=<r> per=<p> old_alive_after_gc=0
// Args: [rounds] [per] [age] (default 8, 2000, 4).
// Engagement (stderr, --verbose:gc): `majors_by_request=` on
// `[GC] zgc-gen-cost:` must be > 0 on a CRATONVM_ZGC_GENERATIONAL=1 run.
import java.lang.ref.WeakReference;

public class Zt3GnSystemGcMajor {
    static volatile Object sink;

    @SuppressWarnings("unchecked")
    static WeakReference<int[]>[] make(Object[] holder) {
        WeakReference<int[]>[] refs = new WeakReference[holder.length];
        for (int i = 0; i < holder.length; i++) {
            int[] referent = new int[16];
            holder[i] = referent;
            refs[i] = new WeakReference<>(referent);
        }
        return refs;
    }

    static void churn(int bytes) {
        for (int done = 0; done < bytes; done += 512) {
            sink = new byte[512];
        }
        sink = null;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int per = args.length > 1 ? Integer.parseInt(args[1]) : 2000;
        int age = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        long alive = 0;
        for (int r = 0; r < rounds; r++) {
            Object[] holder = new Object[per];
            WeakReference<int[]>[] refs = make(holder);
            for (int a = 0; a < age; a++) {
                System.gc();
            }
            java.util.Arrays.fill(holder, null);
            holder = null;
            churn(1 << 20);
            System.gc();
            for (WeakReference<int[]> w : refs) {
                if (w.get() != null) {
                    alive++;
                }
            }
        }
        System.out.println("Zt3GnSystemGcMajor rounds=" + rounds + " per=" + per
                + " old_alive_after_gc=" + alive);
    }
}
