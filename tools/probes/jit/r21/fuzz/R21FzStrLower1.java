// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: ZCP-3's non-allocating half, the IR
// `toLowerCase` unchanged prefix
// (`CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX`, `K::LowerUnchanged` /
// `K::LowerLocaleUnchanged`).
//
// Risk targeted: the prefix returns the RECEIVER when no LATIN1 byte is in
// `CharacterDataLatin1`'s upper-case set (A-Z, 0xC0..0xD6, 0xD8..0xDE) and
// declines otherwise. Identity (`s.toLowerCase() == s`) is Java-visible.
//   * bytes next to the ranges: 0x40, 0x5B, 0xBF, 0xD7 (the sign: NOT upper),
//     0xDF (sharp s), 0xB5, 0xFF, 0xAA, 0xBA -- all unchanged, identity true;
//   * 0x41, 0x5A, 0xC0, 0xD6, 0xD8, 0xDE -- changed, identity false, value
//     lowered;
//   * the empty string (identity true);
//   * `toLowerCase((Locale) null)` must throw NPE even for "" and for an
//     all-lower string (the JDK tests the locale first);
//   * a Turkish / Lithuanian / Azeri locale: an unchanged string returns
//     itself BEFORE the language is read; a changed one takes the slow path
//     (dotless i rules);
//   * a UTF16 receiver (declines; the JDK answer for an unchanged UTF16
//     string is also identity).
//
// Run:
//   javac -d out R21FzStrLower1.java
//   java -Xmx256m -cp out R21FzStrLower1                                  # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzStrLower1
//   CRATONVM_JIT_IR_STRING_LOWER_UNCHANGED_PREFIX=1 cratonvm -Xmx256m -cp out R21FzStrLower1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm (the default locale
// is fixed by the probe: Locale.setDefault(Locale.ROOT) first).

import java.util.Locale;

public class R21FzStrLower1 {
    static final int WARM = 100_000;

    static String lower(String s) {
        return s.toLowerCase();
    }

    static String lowerIn(String s, Locale l) {
        return s.toLowerCase(l);
    }

    static String hex(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            sb.append(Integer.toHexString(s.charAt(i))).append('.');
        }
        return sb.toString();
    }

    static final String[] IN;

    static {
        int[] bytes = {0x40, 0x5B, 0xBF, 0xD7, 0xDF, 0xB5, 0xFF, 0xAA, 0xBA, 0x41, 0x5A, 0xC0, 0xD6, 0xD8, 0xDE,
            0x61, 0x7A, 0xE0, 0xFE, 0x30, 0x20};
        IN = new String[bytes.length + 8];
        for (int i = 0; i < bytes.length; i++) {
            IN[i] = "ab" + (char) bytes[i] + "cd";
        }
        int k = bytes.length;
        IN[k++] = "";
        IN[k++] = "already lower 123";
        IN[k++] = "Capital";
        IN[k++] = "中lower";
        IN[k++] = "中UPPER";
        IN[k++] = "iı";
        IN[k++] = "TITLE I";
        IN[k++] = "x".repeat(70) + "Y";
    }

    static final Locale[] LOCALES = {Locale.ROOT, Locale.ENGLISH, Locale.forLanguageTag("tr"),
        Locale.forLanguageTag("lt"), Locale.forLanguageTag("az")};

    static String one(String s, Locale l) {
        try {
            String r = l == null ? lower(s) : lowerIn(s, l);
            return (r == s ? "same:" : "new:") + hex(r);
        } catch (NullPointerException e) {
            return "NPE";
        }
    }

    static void rows(String tag) {
        for (int i = 0; i < IN.length; i++) {
            StringBuilder sb = new StringBuilder("row " + tag + " s" + i + " " + one(IN[i], null));
            for (Locale l : LOCALES) {
                sb.append(' ').append(one(IN[i], l));
            }
            try {
                String r = lowerIn(IN[i], null);
                sb.append(" null-locale ").append(hex(r));
            } catch (NullPointerException e) {
                sb.append(" null-locale NPE");
            }
            System.out.println(sb);
        }
    }

    public static void main(String[] args) {
        Locale.setDefault(Locale.ROOT);
        rows("cold");
        long h = 0;
        int same = 0;
        for (int r = 0; r < WARM; r++) {
            String s = IN[r % IN.length];
            String a = lower(s);
            String b = lowerIn(s, LOCALES[r % LOCALES.length]);
            if (a == s) {
                same++;
            }
            if (b == s) {
                same++;
            }
            h = h * 31 + a.hashCode() + b.length();
        }
        System.out.println("warm hash " + h + " same=" + same);
        rows("hot");
        Locale.setDefault(Locale.forLanguageTag("tr"));
        System.out.println("default-tr " + one("TITLE I", null) + " " + one("title", null));
        Locale.setDefault(Locale.ROOT);
        rows("after");
    }
}
