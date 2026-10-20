// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.ManagementFactory;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Retain native capture results across TLAB refills and collections. */
public class RegexCaptureRetentionProbe {
    static volatile String[] keep;

    static long fill(Matcher m, String[] values) {
        long sum = 0;
        for (int i = 0; i < values.length; i++) {
            m.reset();
            if (!m.find()) throw new AssertionError("missing match");
            String s = m.group(1);
            if (s == m.group(1)) throw new AssertionError("capture was interned");
            values[i] = s;
            sum += Long.parseLong(s);
        }
        return sum;
    }

    public static void main(String[] args) {
        int n = args.length == 0 ? 100_000 : Integer.parseInt(args[0]);
        Matcher m = Pattern.compile("(\\d+)").matcher("x12345678y");
        fill(m, new String[5_000]);
        String[] values = new String[n];
        keep = values;
        com.sun.management.ThreadMXBean bean =
            (com.sun.management.ThreadMXBean) ManagementFactory.getThreadMXBean();
        long before = bean.getCurrentThreadAllocatedBytes();
        long sum = fill(m, values);
        long allocated = bean.getCurrentThreadAllocatedBytes() - before;
        // Both calls create a String and backing storage. Compare this figure
        // before/after a constructor change: TLAB bumps must not be charged
        // again as external allocations. Exact sizes depend on the VM layout.
        System.out.println("allocated-per-group=" + allocated / (2L * n));
        System.gc();
        long retained = 0;
        for (int i = 0; i < n; i++) {
            if (!values[i].equals("12345678")) throw new AssertionError("corrupt capture " + i);
            if (i > 0 && values[i] == values[i - 1]) throw new AssertionError("shared capture");
            retained += Long.parseLong(values[i]);
        }
        if (sum != 12345678L * n || retained != sum) throw new AssertionError("checksum");
        System.out.println("retained=" + n + " checksum=" + sum + " fresh=true");
        keep = null;
    }
}
