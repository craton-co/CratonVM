// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.util.ArrayList;

/**
 * gcf f3/c (2026-10-01): is a {@code putstatic null} executed inside an
 * OSR-compiled body seen by the collector's static-field root scan?
 *
 * <p>The question comes from {@code c_d4j_escape} (vf1 failure 5): the last
 * old-gen root census before {@code GenR4W4NativeStringOomProbe}'s uncaught
 * error at line 104 still named two static {@code ArrayList}s, 33 MiB, after
 * {@code made = null; fill = null} had run in the OSR'd {@code main}. Either
 * no major ran after the two stores (the census predates them), or the stores
 * were not visible to the root scan (a leak).
 *
 * <p>{@code phase2} fills both lists in a loop long enough to OSR-compile,
 * takes a {@code WeakReference} to each, nulls both statics INSIDE the same
 * compiled body, and returns, so no frame of it is left to hold either list.
 * {@code main} then collects twice and asks the weak references. Only a
 * static root can keep a list alive at that point.
 *
 * <p>HotSpot ({@code -XX:+UseSerialGC -Xmx256m}) prints
 * <pre>
 *   osr-putstatic-null statics-null=true fill-cleared=true made-cleared=true
 * </pre>
 * A {@code false} in a {@code *-cleared} field with {@code statics-null=true}
 * means a root still names the list after its only reference was nulled:
 * keep the err, rerun with {@code CRATONVM_DBG=oldmark-root-census,root-source}
 * and read the holder's category (static-field = the store was not seen;
 * a {@code tier=ir}/{@code tier=sp} band word = a frame holder).
 */
public final class GcfF3cOsrPutstaticNullProbe {
    static ArrayList<long[]> fill = new ArrayList<>();
    static ArrayList<Object> made = new ArrayList<>();
    static WeakReference<Object> weakFill;
    static WeakReference<Object> weakMade;
    static long sink;

    static void phase2(int n, boolean keep) {
        for (int i = 0; i < n; i++) {
            made.add(Integer.toString(i));
            if ((i & 3) == 0) {
                fill.add(new long[16]);
            }
            sink += i;
        }
        weakFill = new WeakReference<>(fill);
        weakMade = new WeakReference<>(made);
        if (!keep) {
            made = null;
            fill = null;
        }
    }

    /**
     * Arguments: {@code [n] [keep]}. {@code keep} is the positive control: the
     * statics are NOT nulled, so both lists must survive
     * ({@code statics-null=false fill-cleared=false made-cleared=false}); a
     * {@code true} there means the probe cannot see a holder at all.
     */
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 400_000;
        boolean keep = args.length > 1 && args[1].equals("keep");
        phase2(n, keep);
        boolean staticsNull = fill == null && made == null;
        System.gc();
        System.gc();
        System.out.println(new StringBuilder("osr-putstatic-null statics-null=").append(staticsNull)
                .append(" fill-cleared=").append(weakFill.get() == null)
                .append(" made-cleared=").append(weakMade.get() == null).toString());
        System.err.println("[probe] sink=" + sink);
    }
}
