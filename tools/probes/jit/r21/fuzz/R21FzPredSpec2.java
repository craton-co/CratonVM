// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: LOOPS21-2 with the String charAt trap and
// the offset / clamp-length planters sharing one header
// (`CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION`; `charat_trap.rs` keeps the
// COARSE `DespecRegistry::contains` question by design, the planters ask
// `predicate_withdrawn`).
//
// Risk targeted: a header where the coarse consumer (charAt trap) and the
// per-planter consumers disagree about whether the header is withdrawn. The
// loop reads `s.charAt(i + off)` and `a[i + off2]` and `b[min(i, lim)]`; each
// row breaks one of them (SIOOBE with the JDK message, AIOOBE, none) and the
// churn phase alternates them past the trap limit, then re-runs every row.
// Two receivers String vs StringBuilder through CharSequence feed the same
// header from a second kernel (the class guard of a CharSequence site).
//
// Run:
//   javac -d out R21FzPredSpec2.java
//   java -Xmx256m -cp out R21FzPredSpec2                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzPredSpec2
//   CRATONVM_JIT_IR_PREDICATE_OWN_SPECULATION=1 cratonvm -Xmx256m -cp out R21FzPredSpec2
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzPredSpec2 {
    static final int WARM = 40_000;
    static int sTrip;

    static long mixed(String s, int off, int[] a, int off2, int[] b, int lim, int n) {
        long t = 0;
        for (int i = 0; i < n; i++) {
            sTrip = i;
            t += s.charAt(i + off);
            t += a[i + off2];
            t += b[Math.min(i, lim)];
        }
        return t;
    }

    static long seq(CharSequence cs, int off, int n) {
        long t = 0;
        for (int i = 0; i < n; i++) {
            sTrip = i;
            t = t * 3 + cs.charAt(i + off);
        }
        return t;
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static final String S = "the quick brown fox jumps over the lazy dog 0123456789";
    static final String U = "Āā utf16 中文 string with enough chars......";
    static final int[] A = new int[48];
    static final int[] B = new int[8];

    static {
        for (int i = 0; i < A.length; i++) {
            A[i] = i * i;
        }
        for (int i = 0; i < B.length; i++) {
            B[i] = 100 + i;
        }
    }

    interface Row {
        long run();
    }

    static void row(String tag, String name, Row r) {
        sTrip = -1;
        String out;
        try {
            out = Long.toString(r.run());
        } catch (RuntimeException e) {
            out = exc(e);
        }
        System.out.println("row " + tag + " " + name + " -> " + out + " trip=" + sTrip);
    }

    static void rows(String tag) {
        row(tag, "ok", () -> mixed(S, 2, A, 1, B, 7, 40));
        row(tag, "utf16", () -> mixed(U, 2, A, 1, B, 7, 40));
        row(tag, "str-high", () -> mixed(S, 20, A, 1, B, 7, 40));
        row(tag, "str-neg", () -> mixed(S, -1, A, 1, B, 7, 40));
        row(tag, "arr-high", () -> mixed(S, 2, A, 10, B, 7, 40));
        row(tag, "clamp-high", () -> mixed(S, 2, A, 1, B, 8, 40));
        row(tag, "zero-trip", () -> mixed(S, 999, A, 999, B, 999, 0));
        StringBuilder sb = new StringBuilder(S);
        row(tag, "seq-str", () -> seq(S, 3, 30));
        row(tag, "seq-sb", () -> seq(sb, 3, 30));
        row(tag, "seq-str-bad", () -> seq(S, 30, 30));
        row(tag, "seq-sb-bad", () -> seq(sb, 30, 30));
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            h = h * 31 + mixed((r & 1) == 0 ? S : U, r & 3, A, r & 7, B, 7, 30);
            h = h * 31 + seq(S, r & 7, 20);
        }
        System.out.println("warm hash " + h);
        rows("hot");
        StringBuilder sb = new StringBuilder(U);
        int caught = 0;
        for (int round = 0; round < 6; round++) {
            for (int r = 0; r < 4000; r++) {
                h = h * 31 + mixed(S, r & 3, A, r & 7, B, 7, 30);
                h = h * 31 + seq((r & 1) == 0 ? S : sb, r & 7, 20);
            }
            String[] bad = {"str", "arr", "clamp"};
            for (String which : bad) {
                try {
                    if (which.equals("str")) {
                        mixed(S, 40, A, 0, B, 0, 40);
                    } else if (which.equals("arr")) {
                        mixed(S, 0, A, 20, B, 0, 40);
                    } else {
                        mixed(S, 0, A, 0, B, 20, 40);
                    }
                } catch (IndexOutOfBoundsException e) {
                    caught++;
                }
            }
            rows("round" + round);
        }
        System.out.println("churn hash " + h + " caught=" + caught);
        rows("after");
    }
}
