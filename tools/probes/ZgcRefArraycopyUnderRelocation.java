// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// Reference System.arraycopy while ZGC's load barrier is armed (the default
// concurrent relocation profile): each copied word goes through the load
// barrier and is stored colored. Copies forward, overlapping in both
// directions within one array, into an Object[] from a String[] and through
// Arrays.copyOf, while the heap churns so concurrent cycles relocate the
// elements. Every copy is checked element by element against the source.
//
// Usage: ZgcRefArraycopyUnderRelocation [rounds=400]
// Prints deterministic `fact` lines (equal on HotSpot 25) and ZRA-PASS / ZRA-FAIL.
import java.util.Arrays;

public class ZgcRefArraycopyUnderRelocation {
    static final class Box {
        final int v;
        Box(int v) {
            this.v = v;
        }
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int n = 2048;
        Box[] src = new Box[n];
        for (int i = 0; i < n; i++) {
            src[i] = (i % 7 == 0) ? null : new Box(i);
        }
        String[] strs = new String[n];
        for (int i = 0; i < n; i++) {
            strs[i] = "s" + i;
        }
        long bad = 0;
        long sum = 0;
        Object[] churn = new Object[128];
        for (int r = 0; r < rounds; r++) {
            int len = 1 + (r * 37) % (n - 1);
            int off = r % (n - len + 1);
            Box[] dst = new Box[n];
            System.arraycopy(src, off, dst, 0, len);
            for (int i = 0; i < len; i++) {
                if (dst[i] != src[off + i]) bad++;
            }
            // Overlap, to a higher index then to a lower one.
            Box[] ov = Arrays.copyOf(src, n);
            int k = 1 + r % 64;
            System.arraycopy(ov, 0, ov, k, n - k);
            for (int i = k; i < n; i++) {
                if (ov[i] != src[i - k]) bad++;
            }
            System.arraycopy(ov, k, ov, 0, n - k);
            for (int i = 0; i < n - k; i++) {
                if (ov[i] != src[i]) bad++;
            }
            Object[] objs = new Object[n];
            System.arraycopy(strs, 0, objs, 0, n);
            for (int i = 0; i < n; i++) {
                if (objs[i] != strs[i]) bad++;
            }
            for (Box b : dst) {
                if (b != null) sum += b.v;
            }
            // Replace a slice so old elements die and new ones are copied.
            for (int i = r % 16; i < n; i += 16) {
                src[i] = (i % 7 == 0) ? null : new Box(i + r);
            }
            for (int c = 0; c < churn.length; c++) {
                churn[c] = new byte[2048 + (c & 127)];
            }
        }
        System.out.println("fact sum " + sum);
        System.out.println("fact mismatches " + bad);
        System.out.println(bad == 0 ? "ZRA-PASS" : "ZRA-FAIL");
    }
}
