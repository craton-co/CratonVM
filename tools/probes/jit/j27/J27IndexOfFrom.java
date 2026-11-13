// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 1, lane STRRX: facts for CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX
 * (opt-in), the optimizing tier's inline String.indexOf(int, int) prefix.
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in both modes, with the
 * switch =1 and unset. The answers are checked against a charAt reference loop
 * (JDK 25 semantics: fromIndex clamped to [0, length], a negative or invalid
 * code point never found, a supplementary one found as its surrogate pair), so
 * HotSpot prints "fact indexof-from-mismatches 0"; the literal facts are listed
 * in the page (r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md).
 *
 * Timing (stderr, "row <name> <ns/op>") only with -Dj27.time=1:
 * split-comma ("a,b,...".split(","), the String.split(CIZ) fast path whose
 * per-field indexOf(ch, off) the prefix answers) and scan-from.
 */
public final class J27IndexOfFrom {
    static final int WARM = 40_000;

    static int idx(String s, int ch, int from) {
        return s.indexOf(ch, from);
    }

    /** JDK 25 String.indexOf(int, int), by charAt. */
    static int ref(String s, int ch, int from) {
        int len = s.length();
        int i = Math.min(Math.max(from, 0), len);
        if (ch < 0 || ch > 0x10FFFF) {
            return -1;
        }
        if (ch < 0x10000) {
            for (; i < len; i++) {
                if (s.charAt(i) == ch) {
                    return i;
                }
            }
            return -1;
        }
        char hi = Character.highSurrogate(ch);
        char lo = Character.lowSurrogate(ch);
        for (; i < len - 1; i++) {
            if (s.charAt(i) == hi && s.charAt(i + 1) == lo) {
                return i;
            }
        }
        return -1;
    }

    static final String[] STRINGS = {
        "",
        "a",
        "a,b,c,d,e,f,g,h,i,j",
        "the quick brown fox jumps over the lazy dog, twice: the quick brown fox",
        "café ÿéé",
        "файл,путь,файл",
        "A𐐷B\udc37\ud801C😀 \ud801",
        "𐐷",
        "x􏿿y￿lone\ud801",
        "0123456789abcdef0123456789abcdef0123456789abcdefĀ",
    };

    static final int[] NEEDLES = {
        'a', ',', ' ', 'z', 0xe9, 0xff, 0x100, 0x444, 0x430, 0xffff, 0xd801, 0xdc37,
        0x10437, 0x1f600, 0x10ffff, 0x110000, -1, Integer.MIN_VALUE, Integer.MAX_VALUE, 0,
    };

    static final int[] FROMS = {
        Integer.MIN_VALUE, -5, -1, 0, 1, 2, 3, 7, 16, 17, 31, 47, 48, 70, 71, 72, 1000,
        Integer.MAX_VALUE,
    };

    public static void main(String[] args) {
        long sink = 0;
        for (int r = 0; r < WARM; r++) {
            String s = STRINGS[r % STRINGS.length];
            sink += idx(s, NEEDLES[r % NEEDLES.length], FROMS[(r * 7) % FROMS.length]);
        }
        int mismatches = 0;
        int checked = 0;
        for (String s : STRINGS) {
            for (int ch : NEEDLES) {
                for (int from : FROMS) {
                    int got = idx(s, ch, from);
                    int want = ref(s, ch, from);
                    checked++;
                    if (got != want) {
                        mismatches++;
                        if (mismatches <= 10) {
                            System.out.println("mismatch s#" + indexOf(s) + " ch=0x"
                                + Integer.toHexString(ch) + " from=" + from + " got=" + got
                                + " want=" + want);
                        }
                    }
                }
            }
        }
        System.out.println("fact indexof-from-checked " + checked);
        System.out.println("fact indexof-from-mismatches " + mismatches);
        String csv = STRINGS[2];
        System.out.println("fact comma-from-0 " + idx(csv, ',', 0));
        System.out.println("fact comma-from-2 " + idx(csv, ',', 2));
        System.out.println("fact comma-from-neg " + idx(csv, ',', -100));
        System.out.println("fact comma-from-len " + idx(csv, ',', csv.length()));
        System.out.println("fact comma-from-max " + idx(csv, ',', Integer.MAX_VALUE));
        System.out.println("fact utf16-comma-from-5 " + idx(STRINGS[5], ',', 5));
        System.out.println("fact supp-from-0 " + idx(STRINGS[6], 0x10437, 0));
        System.out.println("fact supp-from-2 " + idx(STRINGS[6], 0x10437, 2));
        System.out.println("fact emoji-from-1 " + idx(STRINGS[6], 0x1f600, 1));
        System.out.println("fact lone-high-at-end " + idx(STRINGS[8], 0xd801, 0));
        System.out.println("fact invalid-cp " + idx(STRINGS[8], 0x110000, 0));
        System.out.println("fact negative-ch " + idx(STRINGS[8], -1, 0));
        System.out.println("fact split-comma " + String.join("|", csv.split(",")));
        System.out.println("warm " + (sink != 0x7fff_ffff_ffffL));
        if (Boolean.getBoolean("j27.time")) {
            int n = 2_000_000;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                int acc = 0;
                for (int i = 0; i < n; i++) {
                    acc += csv.split(",").length;
                }
                long t1 = System.nanoTime();
                System.err.println("row split-comma " + (t1 - t0) / n + " " + (acc == 0));
                t0 = System.nanoTime();
                acc = 0;
                String text = STRINGS[3];
                for (int i = 0; i < n; i++) {
                    acc += idx(text, ':', i & 15);
                }
                t1 = System.nanoTime();
                System.err.println("row scan-from " + (t1 - t0) / n + " " + (acc == 0));
            }
        }
    }

    private static int indexOf(String s) {
        for (int i = 0; i < STRINGS.length; i++) {
            if (STRINGS[i] == s) {
                return i;
            }
        }
        return -1;
    }
}
