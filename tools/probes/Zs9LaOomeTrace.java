// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.util.ArrayList;

/**
 * zs s9/la: the shape of the first VM-raised {@code OutOfMemoryError}s
 * (ZGC round zs, wave s9, lane la; {@code CRATONVM_ZGC_OOME_TRACED_POOL}).
 *
 * <p>Six times: fill the heap with 64 KiB chunks until {@code Java heap space}
 * is thrown, keep the error, drop the fill. Then print, per error: whether it
 * has a stack trace, its top frame's method, whether it is the same object as
 * an earlier one, its cause and suppressed count. HotSpot hands out its four
 * preallocated traced errors first (each with a fresh trace, distinct objects),
 * then its traceless default (the same object every time). Deterministic
 * stdout. Usage: {@code java -XX:+UseZGC -Xmx64m Zs9LaOomeTrace}.
 */
public class Zs9LaOomeTrace {
    static ArrayList<byte[]> fill;

    static OutOfMemoryError fillOnce() {
        fill = new ArrayList<>();
        try {
            while (true) {
                fill.add(new byte[64 * 1024]);
            }
        } catch (OutOfMemoryError e) {
            fill = null;
            return e;
        }
    }

    public static void main(String[] args) {
        OutOfMemoryError[] seen = new OutOfMemoryError[6];
        for (int i = 0; i < seen.length; i++) {
            seen[i] = fillOnce();
            System.gc();
        }
        for (int i = 0; i < seen.length; i++) {
            OutOfMemoryError e = seen[i];
            int same = -1;
            for (int j = 0; j < i; j++) {
                if (seen[j] == e) {
                    same = j;
                    break;
                }
            }
            StackTraceElement[] st = e.getStackTrace();
            String top = st.length == 0 ? "-" : st[0].getClassName() + "." + st[0].getMethodName();
            System.out.println("oome " + i + " msg=" + e.getMessage() + " traced=" + (st.length > 0)
                    + " top=" + top + " sameAs=" + same + " cause=" + e.getCause()
                    + " suppressed=" + e.getSuppressed().length);
        }
    }
}
