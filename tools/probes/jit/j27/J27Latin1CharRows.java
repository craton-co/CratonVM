// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 4 (lane LATIN1): `Character.isDigit` / `isLetter` /
// `isLetterOrDigit` / `isWhitespace` / `isUpperCase` / `isLowerCase`, both the
// `(C)Z` and the `(I)Z` overload, answered inline from the JDK's
// `CharacterDataLatin1` tables for a Latin-1 code point and by the call for
// every other one (CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND=1, default OFF).
// Page: docs/feature-designs/jit-ir-optimizations.md
//
// Rows (one `fact` line each; checksums are `n` = how many code points the
// predicate admits and `h = h * 31 + cp` over them, in ascending order):
//   latin1 <p>   0..255 through the (I)Z overload, 20 000 hot repetitions
//                (`bad` counts repetitions whose checksum differed);
//   masked <p>   the 256 bytes as `b & 0xFF` (the proven-Latin-1 shape);
//   char <p>     0..0xFFFF through the (C)Z overload, 3 repetitions;
//   all <p>      -16 .. 0x11000F through the (I)Z overload (negative and
//                beyond-U+10FFFF code points included), 3 repetitions;
//   regex-*      Pattern / split / stream rows over one mixed string.
//
//   java -cp <dir> J27Latin1CharRows > hs.txt
//   cratonvm --cp <dir> J27Latin1CharRows | diff - hs.txt
//     x {default, CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND=1,
//        CRATONVM_JIT_IR_LATIN1_CHAR_CLASS_EXPAND=1 CRATONVM_JIT_IR_SCALAR_INTRINSICS=0}
//     x {--real-jdk, --compatible}
//   engagement (switch on): + CRATONVM_DBG_JITC=1 2>&1 | grep 'Latin-1 table inline'
//     expect `[ir] Character.isDigit Latin-1 table inline ... (call on the non-Latin-1 arm)`
//     for latin1Digit / allDigit and `(proven Latin-1, no call)` for maskedDigit.
//   timing (optional): -Dj27.time=true prints `time <row> <ns>` lines after the facts.
//
// Expected HotSpot 25 output: see the page (latin1 / masked / regex lines are
// computed from the JDK 25 tables; char / all lines are "(host)").
public class J27Latin1CharRows {
    static final String[] NAMES = {
        "isDigit", "isLetter", "isLetterOrDigit", "isWhitespace", "isUpperCase", "isLowerCase"
    };

    static boolean test(int p, int cp) {
        switch (p) {
            case 0: return Character.isDigit(cp);
            case 1: return Character.isLetter(cp);
            case 2: return Character.isLetterOrDigit(cp);
            case 3: return Character.isWhitespace(cp);
            case 4: return Character.isUpperCase(cp);
            default: return Character.isLowerCase(cp);
        }
    }

    static boolean testChar(int p, char ch) {
        switch (p) {
            case 0: return Character.isDigit(ch);
            case 1: return Character.isLetter(ch);
            case 2: return Character.isLetterOrDigit(ch);
            case 3: return Character.isWhitespace(ch);
            case 4: return Character.isUpperCase(ch);
            default: return Character.isLowerCase(ch);
        }
    }

    // One monomorphic loop per predicate, so each call site is one
    // `invokestatic` of one triple in a hot body.
    static long latin1Digit(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isDigit(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long latin1Letter(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isLetter(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long latin1LetterOrDigit(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isLetterOrDigit(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long latin1Whitespace(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isWhitespace(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long latin1UpperCase(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isUpperCase(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long latin1LowerCase(int lo, int hi) {
        int n = 0, h = 0;
        for (int cp = lo; cp <= hi; cp++) { if (Character.isLowerCase(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long range(int p, int lo, int hi) {
        switch (p) {
            case 0: return latin1Digit(lo, hi);
            case 1: return latin1Letter(lo, hi);
            case 2: return latin1LetterOrDigit(lo, hi);
            case 3: return latin1Whitespace(lo, hi);
            case 4: return latin1UpperCase(lo, hi);
            default: return latin1LowerCase(lo, hi);
        }
    }

    static long maskedDigit(byte[] bs) {
        int n = 0, h = 0;
        for (byte b : bs) { int cp = b & 0xFF; if (Character.isDigit(cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long masked(int p, byte[] bs) {
        if (p == 0) {
            return maskedDigit(bs);
        }
        int n = 0, h = 0;
        for (byte b : bs) { int cp = b & 0xFF; if (test(p, cp)) { n++; h = h * 31 + cp; } }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static long chars(int p) {
        int n = 0, h = 0;
        for (int i = 0; i <= 0xFFFF; i++) {
            char ch = (char) i;
            if (testChar(p, ch)) { n++; h = h * 31 + i; }
        }
        return ((long) n << 32) | (h & 0xFFFFFFFFL);
    }

    static String fmt(long v) {
        return "n=" + (v >>> 32) + " h=" + (int) v;
    }

    public static void main(String[] args) {
        boolean time = Boolean.getBoolean("j27.time");
        StringBuilder times = new StringBuilder();
        byte[] bytes = new byte[256];
        for (int i = 0; i < 256; i++) bytes[i] = (byte) i;
        for (int p = 0; p < NAMES.length; p++) {
            long t0 = System.nanoTime();
            long first = range(p, 0, 255);
            int bad = 0;
            for (int rep = 1; rep < 20000; rep++) {
                if (range(p, 0, 255) != first) bad++;
            }
            long t1 = System.nanoTime();
            System.out.println("fact latin1 " + NAMES[p] + " " + fmt(first) + " bad=" + bad);
            long m = masked(p, bytes);
            int mbad = 0;
            for (int rep = 1; rep < 20000; rep++) {
                if (masked(p, bytes) != m) mbad++;
            }
            System.out.println("fact masked " + NAMES[p] + " " + fmt(m) + " bad=" + mbad);
            long c = chars(p);
            int cbad = 0;
            for (int rep = 1; rep < 3; rep++) {
                if (chars(p) != c) cbad++;
            }
            System.out.println("fact char " + NAMES[p] + " " + fmt(c) + " bad=" + cbad);
            long a = range(p, -16, 0x11000F);
            int abad = 0;
            for (int rep = 1; rep < 3; rep++) {
                if (range(p, -16, 0x11000F) != a) abad++;
            }
            System.out.println("fact all " + NAMES[p] + " " + fmt(a) + " bad=" + abad);
            times.append("time latin1-").append(NAMES[p]).append(' ').append(t1 - t0).append('\n');
        }
        regexRows();
        if (time) System.out.print(times);
    }

    // Mixed text: ASCII, Latin-1 (NBSP U+00A0, E-acute, e-acute), and
    // non-Latin-1 (LINE SEPARATOR, dotless i, ARABIC-INDIC DIGIT THREE, OMEGA).
    static final String TEXT =
        "Hello World 42\tfoo BAR baz_9 Été ı٣Ωx";

    static int count(java.util.regex.Pattern p, String s) {
        java.util.regex.Matcher m = p.matcher(s);
        int n = 0;
        while (m.find()) n++;
        return n;
    }

    static void regexRows() {
        java.util.regex.Pattern ws = java.util.regex.Pattern.compile("\\p{javaWhitespace}+");
        java.util.regex.Pattern digit = java.util.regex.Pattern.compile("\\p{javaDigit}+");
        java.util.regex.Pattern upper = java.util.regex.Pattern.compile("\\p{javaUpperCase}");
        java.util.regex.Pattern lower = java.util.regex.Pattern.compile("\\p{javaLowerCase}");
        java.util.regex.Pattern lod = java.util.regex.Pattern.compile("\\p{javaLetterOrDigit}");
        int splitN = 0, splitH = 0, d = 0, u = 0, l = 0, ld = 0;
        long letters = 0, letterCps = 0;
        int bad = 0;
        for (int rep = 0; rep < 5000; rep++) {
            String[] parts = ws.split(TEXT);
            int h = String.join("|", parts).hashCode();
            int dd = count(digit, TEXT), uu = count(upper, TEXT);
            int ll = count(lower, TEXT), lld = count(lod, TEXT);
            long le = TEXT.chars().filter(Character::isLetter).count();
            long lc = TEXT.codePoints().filter(Character::isLetter).count();
            if (rep == 0) {
                splitN = parts.length; splitH = h; d = dd; u = uu; l = ll; ld = lld;
                letters = le; letterCps = lc;
            } else if (parts.length != splitN || h != splitH || dd != d || uu != u
                    || ll != l || lld != ld || le != letters || lc != letterCps) {
                bad++;
            }
        }
        System.out.println("fact regex-split-ws n=" + splitN + " h=" + splitH + " bad=" + bad);
        System.out.println("fact regex-digit-runs " + d);
        System.out.println("fact regex-upper " + u);
        System.out.println("fact regex-lower " + l);
        System.out.println("fact regex-letter-or-digit " + ld);
        System.out.println("fact stream-is-letter chars=" + letters + " codepoints=" + letterCps);
    }
}
