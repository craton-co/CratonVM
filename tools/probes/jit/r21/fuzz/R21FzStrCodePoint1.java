// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: CP21-1, the IR `String.codePointAt` prefix
// (`CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX`, `ir_lower`
// `try_emit_ir_string_search_prefix` arm `K::CodePointAt`).
//
// Risk targeted: the prefix answers LATIN1 `value[index] & 0xff` and the
// UTF16 surrogate-pair rule inline and declines every index outside the
// range (the call throws). Edges:
//   * index = -1, length, length + 1, Integer.MIN_VALUE / MAX_VALUE (the
//     zero-extension of a negative index must decline, never wrap);
//   * UTF16: a high surrogate at the LAST index (no successor), a high
//     followed by a non-low, a high followed by another high, a lone low,
//     a full pair at the last two units, U+10FFFF, U+10000;
//   * LATIN1 bytes >= 0x80 (must not sign-extend);
//   * the empty string;
//   * Strings from every construction route (literal, substring, concat,
//     StringBuilder, char[] that compacts, String.valueOf(int)).
// The exception class and message are part of every row (HotSpot's SIOOBE
// text). Rows cold, hot, after a deopt trigger (a CharSequence site fed a
// late implementation, plus a rare exception).
//
// Run:
//   javac -d out R21FzStrCodePoint1.java
//   java -Xmx256m -cp out R21FzStrCodePoint1                              # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzStrCodePoint1
//   CRATONVM_JIT_IR_STRING_CODE_POINT_AT_PREFIX=1 cratonvm -Xmx256m -cp out R21FzStrCodePoint1
//   cratonvm -XX:-CompactStrings ... (if the launcher accepts it; else skip)
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzStrCodePoint1 {
    static final int WARM = 60_000;

    static int cp(String s, int i) {
        return s.codePointAt(i);
    }

    static long sumCp(String s) {
        long t = 0;
        for (int i = 0; i < s.length(); i++) {
            t = t * 1_000_003L + s.codePointAt(i);
        }
        return t;
    }

    static String exc(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static String one(String s, int i) {
        try {
            return Integer.toHexString(cp(s, i));
        } catch (RuntimeException e) {
            return exc(e);
        }
    }

    static String[] inputs() {
        char[] mixed = {'a', 'é', 'ÿ', '\u0080'};
        StringBuilder sb = new StringBuilder();
        sb.append("xþ").appendCodePoint(0x1F600).append('y');
        return new String[] {
            "",
            "a",
            "ÿ\u0080éz",
            new String(mixed),
            "😀",                // pair
            "ab\uD83D",                    // high at the last index
            "\uD83Dx",                     // high then non-low
            "\uD83D😀",          // high, high, low
            "\uDE00a",                     // lone low
            "z􏿿",               // U+10FFFF at the end
            "𐀀",                // U+10000
            "abc😀".substring(3),
            ("pre" + "中") + "\uD83D",
            sb.toString(),
            String.valueOf(-12345),
            new String(new char[] {'\uD83D', '\uDE00', 'q'}, 1, 2),
        };
    }

    static void rows(String tag) {
        String[] in = inputs();
        int[] extra = {-1, Integer.MIN_VALUE, Integer.MAX_VALUE};
        for (int k = 0; k < in.length; k++) {
            String s = in[k];
            StringBuilder out = new StringBuilder("row " + tag + " s" + k + " len=" + s.length() + " :");
            for (int i = 0; i <= s.length() + 1; i++) {
                out.append(' ').append(one(s, i));
            }
            for (int e : extra) {
                out.append(" | ").append(one(s, e));
            }
            if (s.length() > 0) {
                out.append(" sum=").append(sumCp(s));
            }
            System.out.println(out);
        }
    }

    static final class LateSeq implements CharSequence {
        public int length() {
            return 2;
        }

        public char charAt(int i) {
            return (char) ('A' + i);
        }

        public CharSequence subSequence(int a, int b) {
            return this;
        }

        @Override
        public String toString() {
            return "AB";
        }
    }

    static int viaSeq(CharSequence cs, int i) {
        return Character.codePointAt(cs, i);
    }

    public static void main(String[] args) {
        rows("cold");
        String[] in = inputs();
        long h = 0;
        for (int r = 0; r < WARM; r++) {
            String s = in[4 + (r % (in.length - 4))];
            int i = r % s.length();
            h = h * 31 + cp(s, i);
            if ((r & 15) == 0) {
                h += sumCp(s);
            }
            h += viaSeq(s, i);
        }
        System.out.println("warm hash " + h);
        rows("hot");
        System.out.println("late " + viaSeq(new LateSeq(), 1) + " " + viaSeq("😀", 0));
        try {
            cp(null, 0);
        } catch (NullPointerException e) {
            System.out.println("null NPE");
        }
        rows("after");
    }
}
