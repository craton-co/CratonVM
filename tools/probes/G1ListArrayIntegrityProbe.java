// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
import java.util.ArrayList;

/** Check the contents, not just reclamation, of a compiled heap-filling list. */
public final class G1ListArrayIntegrityProbe {
    static ArrayList<long[]> data;
    static long sink;

    static void fill(int cap) {
        for (int i = 0; ; i++) {
            long[] a = new long[128];
            a[0] = i;
            a[127] = ~((long) i);
            data.add(a);
            if (cap >= 0 && i >= cap) throw new OutOfMemoryError("warm-up");
            if (cap < 0 && (i & 1023) == 1023) verify(Math.max(0, i - 1023));
        }
    }

    static void verify(int from) {
        for (int i = from; i < data.size(); i++) {
            long[] a = data.get(i);
            if (a == null || a.length != 128 || a[0] != i || a[127] != ~((long) i)) {
                throw new AssertionError("corrupt list element " + i + " of " + data.size()
                    + " len=" + (a == null ? -1 : a.length)
                    + " value=" + (a == null || a.length == 0 ? -1 : a[0]));
            }
            sink += a[0];
        }
    }

    public static void main(String[] args) {
        for (int w = 0; w < 20_000; w++) {
            data = new ArrayList<>();
            try { fill(3); } catch (OutOfMemoryError expected) { }
        }
        data = new ArrayList<>();
        try { fill(-1); } catch (OutOfMemoryError expected) {
            for (int i = 0; i < 128 && !data.isEmpty(); i++) data.remove(data.size() - 1);
        }
        verify(0);
        data = null;
        System.gc();
        System.gc();
        System.out.println("PASS list-integrity");
    }
}
