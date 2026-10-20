// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: STRINGS21-2, the IR
// `String.compareToIgnoreCase` prefix
// (`CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX`, `K::CompareToIgnoreCase`,
// `Lowerer::emit_latin1_ci_fold` / `emit_latin1_ci_lim`).
//
// Risk targeted: the branch-coded Latin-1 fold `toLowerCase(toUpperCase(b))`
// and its exceptions: 0xB5 (micro sign: upper U+039C, so the fold is U+03BC
// and the ANSWER is `0x3BC - other`, far outside a byte), 0xFF (upper
// U+0178, folds back to 0xFF), 0xDF (sharp s, itself), 0xD7 / 0xF7 (the
// signs, never folded), 0xAA / 0xBA (ordinals, themselves), the A-Z and
// 0xC0..0xDE ranges. Also: the answer when the common prefix is equal
// (`len1 - len2`, negative and positive), equal bytes that are skipped
// before the fold, a fold-equal pair followed by a different pair (the
// reload of the bound after an equal fold), a null argument (NPE from the
// call), a UTF16 receiver or argument (declines to the call), the empty
// string. The full 256 x 256 single-character sweep is checksummed, and the
// rows of the special bytes are printed one by one.
//
// Run:
//   javac -d out R21FzStrCompareCI1.java
//   java -Xmx256m -cp out R21FzStrCompareCI1                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzStrCompareCI1
//   CRATONVM_JIT_IR_STRING_COMPARE_TO_CI_PREFIX=1 cratonvm -Xmx256m -cp out R21FzStrCompareCI1
//   CRATONVM_JIT_IR_STRING_INTERNAL_ADMIT=1 cratonvm ... (the comparator through the JDK body)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzStrCompareCI1 {
    static final String[] ONE = new String[256];

    static {
        for (int i = 0; i < 256; i++) {
            ONE[i] = String.valueOf((char) i);
        }
    }

    static int ci(String a, String b) {
        return a.compareToIgnoreCase(b);
    }

    static long sweep() {
        long h = 0;
        for (int i = 0; i < 256; i++) {
            for (int j = 0; j < 256; j++) {
                h = h * 1_000_003L + ci(ONE[i], ONE[j]);
            }
        }
        return h;
    }

    static final int[] SPECIAL = {0x41, 0x5A, 0x61, 0x7A, 0x40, 0x5B, 0x60, 0x7B, 0xAA, 0xB5, 0xBA, 0xC0, 0xD6,
        0xD7, 0xD8, 0xDE, 0xDF, 0xE0, 0xF6, 0xF7, 0xF8, 0xFE, 0xFF};

    static String exc(Throwable t) {
        return t.getClass().getName();
    }

    static String safe(String a, String b) {
        try {
            return Integer.toString(ci(a, b));
        } catch (RuntimeException e) {
            return exc(e);
        }
    }

    static void rows(String tag) {
        StringBuilder sb = new StringBuilder("row " + tag + " special");
        for (int x : SPECIAL) {
            for (int y : SPECIAL) {
                sb.append(' ').append(ci(ONE[x], ONE[y]));
            }
        }
        System.out.println(sb);
        String[][] pairs = {
            {"", ""}, {"", "a"}, {"a", ""}, {"abc", "ABC"}, {"abc", "ABD"}, {"abcd", "ABC"},
            {"ABC", "abcd"}, {"µx", "µy"}, {"µ", "a"}, {"a", "µ"}, {"µ", "ÿ"},
            {"ÿ", "Ÿ"}, {"µ", "μ"}, {"µ", "Μ"}, {"ß", "SS"},
            {"Àb", "àc"}, {"×", "÷"}, {"AaÉZ", "aAéz"}, {"xA", "xa"},
            {"STRASSE", "straße"}, {"中", "中"}, {"a中", "A丮"}, {"q", null},
        };
        StringBuilder p = new StringBuilder("row " + tag + " pairs");
        for (String[] pr : pairs) {
            p.append(' ').append(safe(pr[0], pr[1]));
        }
        System.out.println(p);
        System.out.println("row " + tag + " sweep " + sweep());
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        String[] words = {"alpha", "ALPHA", "Beta", "beta2", "Été", "éTÉ", "µm", "MU", "", "z"};
        for (int r = 0; r < 200_000; r++) {
            h = h * 31 + ci(words[r % words.length], words[(r / words.length) % words.length]);
        }
        System.out.println("warm hash " + h);
        rows("hot");
        try {
            ci(null, "a");
        } catch (NullPointerException e) {
            System.out.println("null receiver NPE");
        }
        String u = "Āabc";
        System.out.println("utf16 " + ci(u, "āABC") + " " + ci("abc", u) + " " + ci(u.substring(1), "ABC"));
        rows("after");
    }
}
