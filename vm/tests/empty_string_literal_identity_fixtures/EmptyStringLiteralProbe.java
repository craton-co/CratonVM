// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// JDK 25 answers the pooled "" literal (an `ldc`) for an empty builder's
// toString() and for an empty String.repeat. HotSpot C2 may fuse a builder
// chain into a fresh empty String; interpretation keeps the literal. Count
// identities through warm-up and also verify every result remains empty.
public class EmptyStringLiteralProbe {
    static final int N = 50_000;

    public static void main(String[] args) {
        String e = "";
        boolean empty = true;
        int sb = 0, sb16 = 0, setLength = 0, repeat0 = 0, repeatEmpty = 0, repeat1 = 0;
        StringBuilder grown = new StringBuilder("seed");
        String one = "x";
        for (int i = 0; i < N; i++) {
            String a = new StringBuilder().toString();
            String b = new StringBuilder(16).toString();
            if (a == e) sb++;
            if (b == e) sb16++;
            if (a.length() != 0 || b.length() != 0) empty = false;
            grown.append(i);
            grown.setLength(0);
            if (grown.toString() == e) setLength++;
            if (one.repeat(0) == e) repeat0++;
            if (e.repeat(i % 3 + 2) == e) repeatEmpty++;
            if (one.repeat(1) == one) repeat1++;
        }
        System.out.println("sb=" + sb + " sb16=" + sb16 + " setLength=" + setLength
                + " repeat0=" + repeat0 + " repeatEmpty=" + repeatEmpty + " repeat1=" + repeat1
                + " of " + N);
        System.out.println("empty=" + empty);
        // A non-empty builder's String is a fresh object, never the literal.
        String abc = new StringBuilder("ab").append('c').toString();
        System.out.println("fresh=" + (abc != "abc") + " equal=" + abc.equals("abc"));
    }
}
