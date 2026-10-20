// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * gcj j2/ob2 -- the capture probe for HotSpot 25's String-table output
 * ({@code docs/known-issues/gc/gcj-j1st-string-table-logging-and-statistics-are-not-hotspots-20261019.md}
 * items 2 and 3): interns {@code N} fresh Strings, drops every other one,
 * collects twice, and exits. Run on HotSpot with
 * {@code -XX:+PrintStringTableStatistics} (the exit block) and with
 * {@code -Xlog:stringtable*=debug} (which lines a Serial heap prints, and
 * when) to record the texts a CratonVM implementation must reproduce.
 *
 * stdout (deterministic): {@code kept <N/2>}, {@code done}. Everything the
 * flags print is the capture.
 */
public class Gcj2Ob2StringTableChurn {
    static String[] keep;

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        keep = new String[n];
        for (int i = 0; i < n; i++) {
            keep[i] = ("gcj2ob2-churn-" + i + "-" + (i * 17 + 3)).intern();
        }
        int kept = 0;
        for (int i = 0; i < n; i++) {
            if ((i & 1) == 1) {
                keep[i] = null;
            } else {
                kept++;
            }
        }
        System.gc();
        System.gc();
        System.out.println("kept " + kept);
        System.out.println("done");
    }
}
