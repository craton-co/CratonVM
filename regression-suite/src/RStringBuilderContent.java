/**
 * Does a {@code StringBuilder} actually hold what was appended to it?
 *
 * <p>Nothing in this corpus asked before. {@code H14-3} priced
 * {@code register_string_builder_natives} — <b>192 registrations, the largest
 * single registrar in the {@code java.lang} block</b> — at <i>zero vectors</i>,
 * and {@code H22} then measured what that zero concealed: armed across the
 * three classes the registrar covers, every {@code append} was silently
 * discarded and {@code toString()} returned the empty string, with no exception
 * and {@code rc=0}. A suite that never asserts the CONTENT of a built string
 * cannot tell that apart from success, so the zero was not evidence the code
 * was right — it was evidence the corpus was not looking. That is trap 5 of the
 * worker briefs, in the place it cost the most.
 *
 * <p>Every row here is a VALUE assertion. The failure message names the
 * expected and actual text, so a regression reads as
 * {@code sb.toString: expected [abc1true2xy] got []} rather than as a silent
 * {@code rc=0}.
 *
 * <p>Three groups, and each exists for a mechanism:
 * <ul>
 *   <li><b>content</b> — the append overloads, the constructors and
 *       {@code toString}. This is the group that goes red for the
 *       {@code H22} defect.</li>
 *   <li><b>the compact-layout accessors</b> — {@code getChars},
 *       {@code codePointAt}, and {@code String.contentEquals(CharSequence)},
 *       which reaches {@code getValue()}/{@code getCoder()}. Those two natives
 *       exist (BUG-TC0622) precisely so real JDK bytecode can read a builder
 *       whose payload is CratonVM's synthetic {@code char[]}; nothing checked
 *       that they still answer consistently with the content.</li>
 *   <li><b>surrogates</b> — a code point above the BMP must survive as one code
 *       point across {@code appendCodePoint}, {@code length},
 *       {@code codePointAt} and {@code codePointCount}. A builder that stores
 *       {@code cp as u16} passes a length check and fails these.</li>
 * </ul>
 */
public class RStringBuilderContent {
    static int checks = 0;

    static void eq(String what, String got, String want) {
        checks++;
        if (!want.equals(got)) {
            throw new AssertionError(what + ": expected [" + want + "] got [" + got + "]");
        }
    }

    /**
     * All four rendering doors over one array: {@code append(Object)},
     * {@code valueOf(Object)}, string concatenation and {@code toString()} must
     * agree, and all four must be the identity form {@code <descriptor>@<hex>}.
     *
     * <p>The hash is deliberately not compared across doors as text beyond the
     * prefix — it IS the same object in all four, so they must match each other
     * exactly, and that equality is the assertion. Only the PREFIX is compared
     * to a literal, so nothing here depends on the host.
     */
    static void arrayIdentity(String what, Object array, String prefix) {
        String viaToString = array.toString();
        ck(what + ".toString prefix", viaToString.startsWith(prefix),
                "expected " + prefix + "<hash>, got [" + viaToString + "]");
        ck(what + ".toString has a hash", viaToString.length() > prefix.length(),
                "identity rendering is missing its hash: [" + viaToString + "]");
        eq(what + " via append(Object)", new StringBuilder().append(array).toString(), viaToString);
        eq(what + " via String.valueOf(Object)", String.valueOf(array), viaToString);
        eq(what + " via concat", "" + array, viaToString);
        // The insert door reads the same slots through `charsequence_fast_units`.
        eq(what + " via insert(int, Object)",
                new StringBuilder("<>").insert(1, array).toString(), "<" + viaToString + ">");
    }

    static void eqi(String what, int got, int want) {
        checks++;
        if (got != want) {
            throw new AssertionError(what + ": expected " + want + " got " + got);
        }
    }

    static void ck(String what, boolean ok, String detail) {
        checks++;
        if (!ok) {
            throw new AssertionError(what + ": " + detail);
        }
    }

    public static void main(String[] args) {
        // ---- content -------------------------------------------------------
        StringBuilder sb = new StringBuilder();
        sb.append("ab").append('c').append(1).append(true).append(2L)
          .append(new char[] {'x', 'y'});
        eq("sb.toString", sb.toString(), "abc1true2xy");
        eqi("sb.length", sb.length(), 11);
        eqi("sb.charAt(0)", sb.charAt(0), 'a');
        eq("sb.substring", sb.substring(0, 3), "abc");
        eqi("sb.indexOf", sb.indexOf("c1"), 2);
        eq("String.valueOf(sb)", String.valueOf(sb), "abc1true2xy");

        eq("sb.reverse", new StringBuilder("abc").reverse().toString(), "cba");
        eq("sb.insert", new StringBuilder("ac").insert(1, 'b').toString(), "abc");
        eq("sb.delete", new StringBuilder("abcd").delete(1, 3).toString(), "ad");
        eq("sb.replace", new StringBuilder("abcd").replace(1, 3, "XY").toString(), "aXYd");
        eq("sb.ctor(String)", new StringBuilder("seed").toString(), "seed");
        eq("sb.ctor(CharSequence)", new StringBuilder((CharSequence) "seed2").toString(), "seed2");
        eq("sb.ctor(int)+append", new StringBuilder(64).append("cap").toString(), "cap");
        eq("sb.appendCodePoint", new StringBuilder().appendCodePoint(0x41).toString(), "A");
        eq("sb.append(null)", new StringBuilder().append((String) null).toString(), "null");
        eq("sb.repeat(CharSequence,int)", new StringBuilder().repeat("ab", 3).toString(), "ababab");
        eq("sb.repeat(int,int)", new StringBuilder().repeat('z', 3).toString(), "zzz");
        eq("sb.chained", new StringBuilder().append("x").append("y").append("z").toString(), "xyz");
        // javac lowers this to StringConcatFactory, which is a different door
        // into the same layout.
        eq("concat-lowered", ("p" + 1 + 'q' + true), "p1qtrue");

        StringBuffer bf = new StringBuffer();
        bf.append("ab").append('c').append(1);
        eq("buf.toString", bf.toString(), "abc1");
        eqi("buf.length", bf.length(), 4);
        eq("buf.reverse", new StringBuffer("abc").reverse().toString(), "cba");
        eq("buf.insert", new StringBuffer("ac").insert(1, 'b').toString(), "abc");
        eq("buf.appendCodePoint", new StringBuffer().appendCodePoint(0x42).toString(), "B");
        eq("buf.ctor(String)", new StringBuffer("seed").toString(), "seed");

        // ---- the compact-layout accessors ----------------------------------
        ck("String.contentEquals(StringBuilder)", "abc1true2xy".contentEquals(sb),
                "a builder holding the same text compared unequal");
        char[] out = new char[3];
        sb.getChars(0, 3, out, 0);
        eq("sb.getChars", new String(out), "abc");
        eqi("sb.codePointAt", sb.codePointAt(0), 'a');
        eqi("sb.capacity>=length", sb.capacity() >= sb.length() ? 1 : 0, 1);

        // ---- surrogates ----------------------------------------------------
        StringBuilder sur = new StringBuilder();
        sur.appendCodePoint(0x1F600);
        eqi("surrogate.length", sur.length(), 2);
        eqi("surrogate.codePointAt", sur.codePointAt(0), 0x1F600);
        eqi("surrogate.codePointCount", sur.codePointCount(0, 2), 1);
        eqi("surrogate.toString.length", sur.toString().length(), 2);
        eqi("surrogate round-trip", sur.toString().codePointAt(0), 0x1F600);

        // ---- an ARRAY is not a String, whatever its component type is ------
        //
        // A reference array has no class of its own in CratonVM's class store,
        // so it carries its COMPONENT's class id: `String[]` answered
        // `java/lang/String` to every `class_id`-based String test on the
        // rendering path. Three of the four such doors -- `append(Object)`,
        // `valueOf(Object)` and the two CharSequence doors -- then read slot 0
        // as `String.value`, which is the array's FIRST ELEMENT POINTER decoded
        // as the (tag, payload) pair of a value cell. `append` answered the
        // EMPTY string and `valueOf` handed the array back AS a String, which
        // NPEs at the next `.length()` on it.
        //
        // Asserted on SHAPE, never on the identity hash: the prefix is the
        // contract, the digits are not. The three doors are also asserted to
        // agree with each other and with `Object.toString()`, which is what
        // makes a single door regressing visible rather than three matching
        // wrong answers.
        arrayIdentity("String[]", new String[] { "y", "n" }, "[Ljava.lang.String;@");
        arrayIdentity("String[0]", new String[0], "[Ljava.lang.String;@");
        arrayIdentity("String[1]", new String[] { "solo" }, "[Ljava.lang.String;@");
        arrayIdentity("Boolean[]", new Boolean[] { true, false }, "[Ljava.lang.Boolean;@");
        arrayIdentity("Object[]", new Object[] { "o" }, "[Ljava.lang.Object;@");
        arrayIdentity("String[][]", new String[][] { { "a" } }, "[[Ljava.lang.String;@");
        arrayIdentity("int[]", new int[] { 1, 2 }, "[I@");
        arrayIdentity("char[]", new char[] { 'a' }, "[C@");

        // NEGATIVE CONTROLS: the String fast paths those four doors exist for
        // must still be exactly what they were.
        Object plain = "text";
        eq("append(Object) over a real String", new StringBuilder().append(plain).toString(), "text");
        eq("valueOf(Object) over a real String", String.valueOf(plain), "text");
        // `String.toString()` returns `this`, so `valueOf` is an IDENTITY on a
        // String -- the property the fast path is there to preserve.
        ck("valueOf(Object) over a String is the identity", String.valueOf(plain) == plain,
                "String.valueOf((Object) s) must return s itself");
        eq("insert(int, CharSequence) over a real String",
                new StringBuilder("ac").insert(1, (CharSequence) "b").toString(), "abc");

        System.out.println("PASS RStringBuilderContent (" + checks + " checks)");
    }
}
