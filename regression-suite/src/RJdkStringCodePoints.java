// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// W7-95 follow-up, `java/lang/String`: the seven Intrinsic triples the
// intrinsic-semantics census measured as divergent against HotSpot 25.
//
//   codePoints ()Ljava/util/stream/IntStream;
//   codePointAt (I)I
//   codePointCount (II)I
//   offsetByCodePoints (II)I
//   repeat (I)Ljava/lang/String;
//   isBlank ()Z
//   regionMatches (ILjava/lang/String;II)Z   (+ the ignoreCase overload)
//
// Discipline this file is written to:
//
//  * This file is pure ASCII. Every fixture string is built from backslash-u
//    escapes, never from a literal non-ASCII character. A lone surrogate
//    cannot survive a source-encoding round trip at all, and a file that
//    holds U+00A0 as a byte is one `-encoding` default away from being a
//    file that holds a space.
//  * Nothing is printed as a character. Every answer is an `int`, a boolean,
//    or `EX:<SimpleName>`; a code point printed as text tells you what the
//    console encoder did, not what the VM computed.
//  * The fixtures validate themselves (`fx.*` rows) before any subject row
//    runs: "this classifier says NO about U+00A0" passes vacuously if the
//    U+00A0 was folded to a space on the way in.
//  * No lambdas and no `Arrays.toString`: the subject here is `String`, and a
//    probe that also depends on invokedynamic reports the union of two
//    subsystems.
//
// Output is one `ok`/`FAIL` line per check plus a final `@@RESULT` line.
// Every expectation was measured on Microsoft OpenJDK 25.0.3+9, not recalled.

public class RJdkStringCodePoints {

    // 'a', U+1F600 GRINNING FACE (D83D DE00), 'b' -- length 4, 3 code points
    static final String ASTRAL = "a\uD83D\uDE00b";
    // a lone HIGH surrogate in the middle -- length 3, 3 code points
    static final String LONE_HI = "x\uD800y";
    // a lone LOW surrogate first -- length 2, 2 code points
    static final String LONE_LO = "\uDC00a";

    static int checks = 0;
    static int fails = 0;

    static void eq(String label, String actual, String expected) {
        checks++;
        if (!expected.equals(actual)) {
            fails++;
            System.out.println("FAIL " + label + " expected=" + expected + " actual=" + actual);
        } else {
            System.out.println("ok   " + label + "=" + actual);
        }
    }

    static String ex(Throwable t) {
        return "EX:" + t.getClass().getSimpleName();
    }

    // ---- one wrapper per triple; no lambdas ------------------------------

    static void cpa(String label, String s, int index, String expected) {
        String got;
        try {
            got = Integer.toString(s.codePointAt(index));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static void cpc(String label, String s, int begin, int end, String expected) {
        String got;
        try {
            got = Integer.toString(s.codePointCount(begin, end));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static void obc(String label, String s, int index, int offset, String expected) {
        String got;
        try {
            got = Integer.toString(s.offsetByCodePoints(index, offset));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static String join(int[] a) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < a.length; i++) {
            if (i > 0) {
                sb.append(',');
            }
            sb.append(a[i]);
        }
        return "[" + sb + "]";
    }

    static void cps(String label, String s, String expected) {
        String got;
        try {
            got = join(s.codePoints().toArray());
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static void chs(String label, String s, String expected) {
        String got;
        try {
            got = join(s.chars().toArray());
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    // repeat is reported by length + hashCode, never by the text itself: a
    // 6-char answer that is the wrong 6 chars must not pass.
    static void rep(String label, String s, int count, String expected) {
        String got;
        try {
            String r = s.repeat(count);
            got = "len:" + r.length() + " hash:" + r.hashCode();
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static void blk(String label, String s, String expected) {
        String got;
        try {
            got = Boolean.toString(s.isBlank());
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    // trim/strip answers are printed as code units, never as text: the whole
    // question is which invisible characters survived.
    static String units(String s) {
        int[] a = new int[s.length()];
        for (int i = 0; i < a.length; i++) {
            a[i] = s.charAt(i);
        }
        return join(a);
    }

    static void trm(String label, String s, String expected) {
        eq(label, units(s.trim()), expected);
    }

    static void stp(String label, String s, String expected) {
        eq(label, units(s.strip()), expected);
    }

    static void stl(String label, String s, String expected) {
        eq(label, units(s.stripLeading()), expected);
    }

    static void str(String label, String s, String expected) {
        eq(label, units(s.stripTrailing()), expected);
    }

    static void rm(String label, String s, int toffset, String other, int ooffset, int len,
            String expected) {
        String got;
        try {
            got = Boolean.toString(s.regionMatches(toffset, other, ooffset, len));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    static void rmi(String label, String s, boolean ic, int toffset, String other, int ooffset,
            int len, String expected) {
        String got;
        try {
            got = Boolean.toString(s.regionMatches(ic, toffset, other, ooffset, len));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    // ---------------------------------------------------------------------

    static void fixtures() {
        // If any of these fail, every row below is measuring the source
        // encoding rather than the VM.
        eq("fx.astral.len", Integer.toString(ASTRAL.length()), "4");
        eq("fx.astral.c1", Integer.toString(ASTRAL.charAt(1)), "55357");
        eq("fx.astral.c2", Integer.toString(ASTRAL.charAt(2)), "56832");
        eq("fx.lonehi.len", Integer.toString(LONE_HI.length()), "3");
        eq("fx.lonehi.c1", Integer.toString(LONE_HI.charAt(1)), "55296");
        eq("fx.lonelo.c0", Integer.toString(LONE_LO.charAt(0)), "56320");
        eq("fx.nbsp", Integer.toString("\u00A0".charAt(0)), "160");
        eq("fx.nel", Integer.toString("\u0085".charAt(0)), "133");
        eq("fx.figsp", Integer.toString("\u2007".charAt(0)), "8199");
        eq("fx.nnbsp", Integer.toString("\u202F".charAt(0)), "8239");
        eq("fx.fs", Integer.toString("\u001C".charAt(0)), "28");
        eq("fx.dotI", Integer.toString("\u0130".charAt(0)), "304");
        eq("fx.kelvin", Integer.toString("\u212A".charAt(0)), "8490");
        eq("fx.sharps", Integer.toString("\u00DF".charAt(0)), "223");
    }

    static void codePointAt() {
        cpa("cpa.astral.0", ASTRAL, 0, "97");
        cpa("cpa.astral.1", ASTRAL, 1, "128512");
        cpa("cpa.astral.2", ASTRAL, 2, "56832");
        cpa("cpa.astral.3", ASTRAL, 3, "98");
        cpa("cpa.lonehi.1", LONE_HI, 1, "55296");
        cpa("cpa.lonelo.0", LONE_LO, 0, "56320");
        cpa("cpa.astral.neg", ASTRAL, -1, "EX:StringIndexOutOfBoundsException");
        cpa("cpa.astral.4", ASTRAL, 4, "EX:StringIndexOutOfBoundsException");
    }

    static void codePointCount() {
        cpc("cpc.astral.0.4", ASTRAL, 0, 4, "3");
        cpc("cpc.astral.1.3", ASTRAL, 1, 3, "1");
        cpc("cpc.astral.1.2", ASTRAL, 1, 2, "1");
        cpc("cpc.lonehi.0.3", LONE_HI, 0, 3, "3");
        cpc("cpc.astral.4.4", ASTRAL, 4, 4, "0");
        cpc("cpc.astral.3.1", ASTRAL, 3, 1, "EX:IndexOutOfBoundsException");
        cpc("cpc.astral.0.9", ASTRAL, 0, 9, "EX:IndexOutOfBoundsException");
        cpc("cpc.astral.n1.2", ASTRAL, -1, 2, "EX:IndexOutOfBoundsException");
    }

    static void offsetByCodePoints() {
        obc("obc.astral.0.2", ASTRAL, 0, 2, "3");
        obc("obc.astral.0.1", ASTRAL, 0, 1, "1");
        obc("obc.astral.0.3", ASTRAL, 0, 3, "4");
        obc("obc.astral.4.0", ASTRAL, 4, 0, "4");
        obc("obc.astral.4.m1", ASTRAL, 4, -1, "3");
        obc("obc.astral.4.m2", ASTRAL, 4, -2, "1");
        obc("obc.astral.0.9", ASTRAL, 0, 9, "EX:IndexOutOfBoundsException");
        obc("obc.astral.0.m1", ASTRAL, 0, -1, "EX:IndexOutOfBoundsException");
        obc("obc.astral.10.m1", ASTRAL, 10, -1, "EX:IndexOutOfBoundsException");
        obc("obc.astral.n1.1", ASTRAL, -1, 1, "EX:IndexOutOfBoundsException");
    }

    static void codePoints() {
        cps("cps.astral", ASTRAL, "[97,128512,98]");
        cps("cps.lonehi", LONE_HI, "[120,55296,121]");
        cps("cps.lonelo", LONE_LO, "[56320,97]");
        cps("cps.ascii", "abc", "[97,98,99]");
        cps("cps.empty", "", "[]");
        // negative control: chars() is the code-UNIT view and must NOT pair
        chs("chs.astral", ASTRAL, "[97,55357,56832,98]");
        chs("chs.lonehi", LONE_HI, "[120,55296,121]");
    }

    static void repeat() {
        rep("rep.ab.3", "ab", 3, "len:6 hash:" + "ababab".hashCode());
        rep("rep.ab.1", "ab", 1, "len:2 hash:" + "ab".hashCode());
        rep("rep.ab.0", "ab", 0, "len:0 hash:0");
        rep("rep.e.0", "", 0, "len:0 hash:0");
        rep("rep.ab.m1", "ab", -1, "EX:IllegalArgumentException");
        rep("rep.e.m1", "", -1, "EX:IllegalArgumentException");
        rep("rep.ab.min", "ab", Integer.MIN_VALUE, "EX:IllegalArgumentException");
        // Overflow: `Integer.MAX_VALUE / count < len` is checked BEFORE any
        // allocation, so this is cheap on HotSpot and must not become a
        // 4 GB allocation (or a Rust capacity-overflow panic) here.
        rep("rep.ab.max", "ab", Integer.MAX_VALUE, "EX:OutOfMemoryError");
        rep("rep.abc.1e9", "abc", 1000000000, "EX:OutOfMemoryError");
    }

    static void isBlank() {
        blk("blk.empty", "", "true");
        blk("blk.sptabnl", " \t\n", "true");
        blk("blk.a", "a", "false");
        blk("blk.mixed", " a ", "false");
        blk("blk.astral", ASTRAL, "false");
        // Java's Character.isWhitespace EXCLUDES every non-breaking space.
        // Rust's char::is_whitespace includes them -- this is the divergence.
        blk("blk.nbsp", "\u00A0", "false");
        blk("blk.nel", "\u0085", "false");
        blk("blk.figsp", "\u2007", "false");
        blk("blk.nnbsp", "\u202F", "false");
        // ...and INCLUDES the four file/group/record/unit separators, which
        // Rust's does not.
        blk("blk.fs1c", "\u001C", "true");
        blk("blk.gs1d", "\u001D", "true");
        blk("blk.rs1e", "\u001E", "true");
        blk("blk.us1f", "\u001F", "true");
        // negative controls: characters both tables agree ARE whitespace
        blk("blk.linesep", "\u2028", "true");
        blk("blk.parasep", "\u2029", "true");
        blk("blk.ogham", "\u1680", "true");
        blk("blk.enquad", "\u2000", "true");
    }

    // trim() and strip() are the SAME defect as isBlank() and a DIFFERENT
    // contract from each other, so they get their own rows: trim() removes
    // chars <= U+0020 (a raw code-unit test that predates Unicode-aware
    // whitespace in Java), strip() uses Character.isWhitespace. Rust's
    // str::trim is neither. Results are printed as code units.
    static void trimStrip() {
        trm("trm.nbsp", "\u00a0x\u00a0", "[160,120,160]");
        stp("stp.nbsp", "\u00a0x\u00a0", "[160,120,160]");
        trm("trm.nul", "\u0000x\u0000", "[120]");
        stp("stp.nul", "\u0000x\u0000", "[0,120,0]");
        trm("trm.fs", "\u001Cx\u001C", "[120]");
        stp("stp.fs", "\u001Cx\u001C", "[120]");
        trm("trm.linesep", "\u2028x\u2028", "[8232,120,8232]");
        stp("stp.linesep", "\u2028x\u2028", "[120]");
        trm("trm.figsp", "\u2007x\u2007", "[8199,120,8199]");
        stp("stp.figsp", "\u2007x\u2007", "[8199,120,8199]");
        stl("stl.figsp", "\u2007x\u2007", "[8199,120,8199]");
        str("str.figsp", "\u2007x\u2007", "[8199,120,8199]");
        trm("trm.mix", " \t x \t ", "[120]");
        stp("stp.mix", " \t x \t ", "[120]");
        stl("stl.mix", " \t x \t ", "[120,32,9,32]");
        str("str.mix", " \t x \t ", "[32,9,32,120]");
    }

    static void regionMatches() {
        rm("rm.same", "ABC", 0, "ABC", 0, 3, "true");
        rm("rm.mid", "ABC", 1, "xBC", 1, 2, "true");
        rm("rm.casediff", "ABC", 0, "abc", 0, 3, "false");
        rm("rm.endzero", "ABC", 3, "abc", 3, 0, "true");
        rm("rm.overrun", "ABC", 2, "abc", 0, 5, "false");
        rm("rm.negtoff", "ABC", -1, "abc", 0, 0, "false");
        rm("rm.negooff", "ABC", 0, "abc", -1, 0, "false");
        rm("rm.hugetoff", "ABC", Integer.MAX_VALUE, "abc", 0, 1, "false");
        // A NEGATIVE len is not an error: the JDK's bounds test passes
        // (`toffset > length() - len` is false when len < 0) and
        // `while (len-- > 0)` runs zero times, so the answer is true. Code
        // that passes a computed length depends on it.
        rm("rm.neglen", "ABC", 0, "abc", 0, -1, "true");
        rm("rm.minlen", "ABC", 0, "abc", 0, Integer.MIN_VALUE, "true");

        rmi("rmi.same", "ABC", true, 0, "abc", 0, 3, "true");
        rmi("rmi.false", "ABC", true, 0, "abd", 0, 3, "false");
        rmi("rmi.overrun", "ABC", true, 0, "abc", 0, 4, "false");
        rmi("rmi.off", "ABC", false, 0, "abc", 0, 3, "false");
        rmi("rmi.neglen", "ABC", true, 0, "abc", 0, -1, "true");
        rmi("rmi.minlen", "ABC", true, 0, "abc", 0, Integer.MIN_VALUE, "true");
        // U+0130 LATIN CAPITAL LETTER I WITH DOT ABOVE. Java's 1:1
        // Character.toLowerCase maps it to 'i', so it matches "i". Rust's
        // char::to_lowercase yields a TWO-char expansion, which compares
        // unequal -- the same mapping-arity defect W7-95 found in
        // Character.toUpperCase.
        rmi("rmi.dotI", "\u0130", true, 0, "i", 0, 1, "true");
        // U+212A KELVIN SIGN lower-cases to 'k' in both tables.
        rmi("rmi.kelvin", "\u212A", true, 0, "k", 0, 1, "true");
        // U+00DF sharp s: Java's 1:1 toUpperCase leaves it alone, so it does
        // NOT match 'S'. A body that takes the first char of the full
        // uppercase mapping ("SS") would answer true here.
        rmi("rmi.sharps", "\u00DF", true, 0, "S", 0, 1, "false");
        // Surrogate code units compare only bit-identically.
        rmi("rmi.lonehi", "\uD800", true, 0, "\uD800", 0, 1, "true");
        rmi("rmi.lonemix", "\uD800", true, 0, "\uDC00", 0, 1, "false");
    }

    public static void main(String[] args) {
        fixtures();
        codePointAt();
        codePointCount();
        offsetByCodePoints();
        codePoints();
        repeat();
        isBlank();
        trimStrip();
        regionMatches();
        System.out.println("@@RESULT checks=" + checks + " fails=" + fails);
        if (fails != 0) {
            throw new RuntimeException(fails + " String code-point/intrinsic checks failed");
        }
    }
}
