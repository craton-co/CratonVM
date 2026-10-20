// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.lang.reflect.Field;

/**
 * gcj j1/pn: does a primitive {@code long} local whose VALUE equals an
 * object's address keep that object alive?
 *
 * <p>HotSpot never treats a primitive as a reference, so the object is
 * collected and the probe prints {@code PASS long-local}. CratonVM's
 * conservative interpreter-locals probe ({@code Frame::scan_locals_conservative})
 * tries every live local's {@code long} payload as a pointer; on the
 * Generational heap {@code CRATONVM_GEN_CONSERVATIVE_LOCALS_SKIP_PRIMITIVE_KINDS=1}
 * makes it honour the slot's {@code long} kind mark, as the precise scan does.
 *
 * <p>The {@code long} is the raw first word of an {@code Object[]} element read
 * through {@code sun.misc.Unsafe.getLong}: on CratonVM that is the element's
 * reference bits (the object's address); on HotSpot with compressed oops it is
 * two narrow oops, which is fine -- only the verdict is printed. The conservative
 * probe runs only while a compiled frame or a conservative peer is live, so
 * {@code main}'s loop is warmed first ({@code warm} rounds) to get compiled.
 *
 * <p>stdout (deterministic): {@code PASS long-local} or {@code FAIL long-local};
 * the per-round count goes to stderr. Args: {@code [rounds] [warm]} (default 20 200000).
 */
public class Gcj1PnLongLocalProbe {
    static sun.misc.Unsafe U;
    static long sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        int warm = args.length > 1 ? Integer.parseInt(args[1]) : 200000;
        Field f = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        U = (sun.misc.Unsafe) f.get(null);
        long acc = 0;
        for (int i = 0; i < warm; i++) {
            acc += spin(i);
        }
        sink = acc;
        int kept = 0;
        for (int r = 0; r < rounds; r++) {
            if (holdAsLong()) {
                kept++;
            }
        }
        System.out.println(kept == 0 ? "PASS long-local" : "FAIL long-local");
        System.err.println("gcj1pn long-local kept=" + kept + " of " + rounds);
    }

    static long spin(int i) {
        return (i * 31L) ^ (i >>> 3);
    }

    /** True when the object named only by a {@code long} local survives a full GC. */
    static boolean holdAsLong() {
        Object[] box = {new byte[64 * 1024]};
        long bits = U.getLong(box, (long) U.arrayBaseOffset(Object[].class));
        WeakReference<Object> w = new WeakReference<>(box[0]);
        box[0] = null;
        box = null;
        System.gc();
        boolean kept = w.get() != null;
        // `bits` stays live across the collection: it is read here.
        sink += bits & 1;
        return kept;
    }
}
