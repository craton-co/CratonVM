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
// OUTPUT CONTRACT. One `CK RJdkStringCodePoints <label>=<actual>` line per
// check, then `CK RJdkStringCodePoints fails=N`, `CK RJdkStringCodePoints
// checks=N`, then `PASS RJdkStringCodePoints (N checks)`. Every line survives
// `run.sh`'s extract() filter and nothing is printed on any other prefix.
//
// It did not used to. Until 2026-08-13 every check printed `ok <label>=<value>`
// and the run ended on `@@RESULT checks=186 fails=0`, and extract() keeps ONLY
// `PASS `/`CK ` lines — so all 187 lines were deleted and the cross-VM diff
// compared TWO EMPTY STRINGS. All three of the surviving guards fired on the
// ORACLE: G4 ("exited 0 but printed no PASS line"), G2 ("nothing survives
// extract()") and G3 (no parseable check count). run.sh's own verdict greps
// `^PASS RJdkStringCodePoints`, so the vector was scored FAIL on any VM
// whatsoever, including a correct one. Measured on HotSpot 25.0.3+9: rc=0,
// 186 checks, fails=0 — the expectations were right all along; only the
// reporting dialect was wrong. This is precisely the W7-51/W7-60 vacuity shape
// the guards exist to catch, caught on the vector's first scheduled run.
//
// The ACTUAL value goes on the CK line, not a bare `ok`: the two VMs then diff
// their answers against each other directly, so a wrong answer is red even if
// this file's own `expected` table were ever wrong. The expected value is
// appended only when the check fails, for the human reading the log.
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

    // The ONE funnel every check goes through, so the prefix is fixed in one
    // place. `CK ` is not decoration: it is what run.sh's extract() keeps.
    static void eq(String label, String actual, String expected) {
        checks++;
        if (!expected.equals(actual)) {
            fails++;
            System.out.println("CK RJdkStringCodePoints " + label + "=" + actual
                    + " WRONG-expected=" + expected);
        } else {
            System.out.println("CK RJdkStringCodePoints " + label + "=" + actual);
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

    // String.indent(n) for NEGATIVE n is
    // `s.substring(Math.min(-n, s.indexOfNonWhitespace()))` -- a count of
    // CHARACTERS over Character.isWhitespace. Implementing it as a count of
    // BYTES over Rust's whitespace both keeps the wrong characters AND slices
    // a str at a non-boundary, which panics and kills the VM. The nbsp row is
    // the panic; only these rows are here, because indent()'s line-terminator
    // rules are a separate defect that RJdkIntrinsics2's strfmt family owns.
    static void indent() {
        ind("ind.nbsp.m1", "\u00A0a", -1, "[160,97,10]");
        ind("ind.linesep.m1", "\u2028a", -1, "[97,10]");
        ind("ind.sp2.m1", "  a", -1, "[32,97,10]");
        ind("ind.sp2.m2", "  a", -2, "[97,10]");
        ind("ind.sp2.m9", "  a", -9, "[97,10]");
        ind("ind.fs.m1", "\u001Ca", -1, "[97,10]");
        ind("ind.tab.m1", "\ta", -1, "[97,10]");
        ind("ind.min", "  a", Integer.MIN_VALUE, "[97,10]");
        ind("ind.empty.1", "", 1, "[]");
    }

    static void ind(String label, String s, int n, String expected) {
        String got;
        try {
            got = units(s.indent(n));
        } catch (Throwable t) {
            got = ex(t);
        }
        eq(label, got, expected);
    }

    // The whitespace table, pinned COMPLETELY rather than sampled.
    // Character.isWhitespace is true for exactly 25 code points on JDK 25
    // (enumerated by sweeping all 1,114,112 and collecting the hits), so the
    // whole set fits in one positive row, and the four Unicode White_Space
    // code points Java EXCLUDES fit in four negative ones. A table this small
    // does not need a sample.
    static final String ALL_JAVA_WS =
            "\t\n\u000B\f\r\u001C\u001D\u001E\u001F\u0020"
            + "\u1680\u2000\u2001\u2002\u2003\u2004\u2005\u2006\u2008\u2009\u200A"
            + "\u2028\u2029\u205F\u3000";

    static void whitespaceTable() {
        eq("ws.count", Integer.toString(ALL_JAVA_WS.length()), "25");
        blk("ws.all", ALL_JAVA_WS, "true");
        // and each one alone, so a single wrong member cannot hide behind the
        // other 24 in the conjunction above
        for (int i = 0; i < ALL_JAVA_WS.length(); i++) {
            char c = ALL_JAVA_WS.charAt(i);
            blk("ws.one." + Integer.toHexString(c), String.valueOf(c), "true");
        }
        // Unicode says White_Space, Java says no. These four are the entire
        // disagreement, and each is a non-breaking space or NEL.
        blk("ws.not.0085", "\u0085", "false");
        blk("ws.not.00a0", "\u00A0", "false");
        blk("ws.not.2007", "\u2007", "false");
        blk("ws.not.202f", "\u202F", "false");
    }

    // Version skew: Rust's Unicode tables are NEWER than the JDK's and
    // case-pair these six; JDK 25 maps every one to itself. regionMatches
    // ignore-case is the discriminator -- it is false on HotSpot and would be
    // true against a table that pairs them.
    static final char[] SKEW = { '\uA7CE', '\uA7CF', '\uA7D2', '\uA7D3', '\uA7D4', '\uA7D5' };

    static void unicodeVersionSkew() {
        // fixtures first: four of the six are UNASSIGNED in JDK 25 and two are
        // assigned lowercase letters with no uppercase partner. If a future JDK
        // assigns them, these rows are the ones that must be re-measured.
        eq("skew.a7ce.def", Boolean.toString(Character.isDefined(0xA7CE)), "false");
        eq("skew.a7d3.def", Boolean.toString(Character.isDefined(0xA7D3)), "true");
        eq("skew.a7d3.type", Integer.toString(Character.getType(0xA7D3)), "2");

        for (int i = 0; i < SKEW.length; i++) {
            String s = String.valueOf(SKEW[i]);
            String tag = Integer.toHexString(SKEW[i]);
            eq("skew.up." + tag, units(s.toUpperCase(java.util.Locale.ROOT)),
                    "[" + (int) SKEW[i] + "]");
            eq("skew.lo." + tag, units(s.toLowerCase(java.util.Locale.ROOT)),
                    "[" + (int) SKEW[i] + "]");
        }
        // the three pairs Rust joins and the JDK does not
        rmi("skew.rm.a7cf", "\uA7CF", true, 0, "\uA7CE", 0, 1, "false");
        rmi("skew.rm.a7d3", "\uA7D3", true, 0, "\uA7D2", 0, 1, "false");
        rmi("skew.rm.a7d5", "\uA7D5", true, 0, "\uA7D4", 0, 1, "false");
        // negative controls: neighbours inside the SAME span that the JDK DOES
        // case-pair, so a blanket range guard over A7CE..A7D5 would break them
        rmi("skew.ctl.a7d1", "\uA7D1", true, 0, "\uA7D0", 0, 1, "true");
        rmi("skew.ctl.a7d7", "\uA7D7", true, 0, "\uA7D6", 0, 1, "true");
        rmi("skew.ctl.a7cd", "\uA7CD", true, 0, "\uA7CC", 0, 1, "true");
        // and the full-mapping path must still expand, i.e. the fix above must
        // not have turned String.toUpperCase into the 1:1 mapping
        eq("skew.sharps.up", units("\u00DF".toUpperCase(java.util.Locale.ROOT)), "[83,83]");
    }

    // The MIDDLE of the domain, not just the corners. W7-95's Math.pow finding
    // was that special values were right while ordinary inputs were 44 ulp
    // wrong, because the census sampled edges. The code-point family's
    // "ordinary input" is any text at all, so walk real blocks end to end and
    // check the four methods against each other and against Character's own
    // arithmetic at EVERY position.
    static void sweep() {
        int[] starts = { 0x0020, 0x00C0, 0x0400, 0x0590, 0x0E00, 0x1E00, 0x3040, 0x4E00,
                0xAC00, 0xA7C0, 0x10000, 0x1F600, 0x20000, 0x10FF00 };
        for (int i = 0; i < starts.length; i++) {
            sweepBlock(starts[i], 64);
        }
    }

    // The sweep's EXPECTATIONS must not come from the VM under test. A code
    // point needs two UTF-16 units iff it is supplementary -- that is
    // arithmetic, not a lookup, so compute it here instead of calling
    // Character.charCount, which is itself a native this suite is auditing.
    // Otherwise a charCount that is wrong in the same direction as
    // codePointAt would make every row below agree vacuously.
    static int cc(int cp) {
        return cp >= 0x10000 ? 2 : 1;
    }

    static void sweepBlock(int start, int count) {
        StringBuilder sb = new StringBuilder();
        int[] cps = new int[count];
        int n = 0;
        int wantUnits = 0;
        for (int cp = start; n < count && cp <= 0x10FFFF; cp++) {
            if (cp >= 0xD800 && cp <= 0xDFFF) {
                continue;
            }
            cps[n++] = cp;
            wantUnits += cc(cp);
            sb.appendCodePoint(cp);
        }
        String s = sb.toString();
        String tag = "sweep." + Integer.toHexString(start);
        String fail = null;

        // [setup lies]: validate the fixture before trusting any row over it.
        // If appendCodePoint is broken, say so instead of blaming codePointAt.
        if (s.length() != wantUnits) {
            fail = "FIXTURE length=" + s.length() + " want " + wantUnits;
        }
        if (fail == null && s.codePointCount(0, s.length()) != n) {
            fail = "codePointCount(0,len)=" + s.codePointCount(0, s.length()) + " want " + n;
        }
        int idx = 0;
        for (int k = 0; k < n && fail == null; k++) {
            if (s.codePointAt(idx) != cps[k]) {
                fail = "codePointAt(" + idx + ")=" + s.codePointAt(idx) + " want " + cps[k];
            } else if (s.offsetByCodePoints(0, k) != idx) {
                fail = "offsetByCodePoints(0," + k + ")=" + s.offsetByCodePoints(0, k)
                        + " want " + idx;
            } else if (s.codePointCount(0, idx) != k) {
                fail = "codePointCount(0," + idx + ")=" + s.codePointCount(0, idx) + " want " + k;
            } else {
                idx += cc(cps[k]);
            }
        }
        if (fail == null && idx != s.length()) {
            fail = "forward walk ended at " + idx + " want " + s.length();
        }
        // backward walk: offsetByCodePoints from the end must retrace it
        for (int k = 0; k <= n && fail == null; k++) {
            int back = s.offsetByCodePoints(s.length(), -k);
            int wantIdx = s.length();
            for (int j = 0; j < k; j++) {
                wantIdx -= cc(cps[n - 1 - j]);
            }
            if (back != wantIdx) {
                fail = "offsetByCodePoints(len,-" + k + ")=" + back + " want " + wantIdx;
            }
        }
        // codePoints() must agree with the walk, element for element
        if (fail == null) {
            int[] got = s.codePoints().toArray();
            if (got.length != n) {
                fail = "codePoints().length=" + got.length + " want " + n;
            } else {
                for (int k = 0; k < n; k++) {
                    if (got[k] != cps[k]) {
                        fail = "codePoints()[" + k + "]=" + got[k] + " want " + cps[k];
                        break;
                    }
                }
            }
        }
        // regionMatches against itself at every offset, and repeat(2) must be
        // exactly the concatenation
        for (int k = 0; k <= s.length() && fail == null; k++) {
            if (!s.regionMatches(k, s, k, s.length() - k)) {
                fail = "regionMatches(self) false at " + k;
            }
        }
        if (fail == null && !(s + s).equals(s.repeat(2))) {
            fail = "repeat(2) != s+s";
        }
        eq(tag, fail == null ? "ok" : fail, "ok");
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
        indent();
        whitespaceTable();
        unicodeVersionSkew();
        sweep();
        regionMatches();
        // `fails` first and on its own line: harness_check_count reads the
        // REST OF THE LINE after `checks=`, so a combined
        // `CK ... checks=186 fails=0` yields the count "186 fails=0" and G3's
        // `[ "$count" -eq 0 ]` test then errors out invisibly instead of
        // comparing a number. One value per line.
        System.out.println("CK RJdkStringCodePoints fails=" + fails);
        System.out.println("CK RJdkStringCodePoints checks=" + checks);
        if (fails != 0) {
            throw new RuntimeException(fails + " String code-point/intrinsic checks failed");
        }
        // Last, and only on the clean path: run.sh treats this line as the
        // vector's verdict. The `@@RESULT` line this replaces was invisible to
        // extract() and to every consumer — nothing under regression-suite/
        // parses `@@RESULT`; the app-suite runners that do (apps/hib-suite-runner)
        // never run this class.
        System.out.println("PASS RJdkStringCodePoints (" + checks + " checks)");
    }
}
