/**
 * Where exactly does `+` lose an unpaired surrogate?
 *
 * `docs/known-issues/string-concat-loses-unpaired-surrogates.md` located the
 * loss in `execute_string_concat`, which accumulated into a Rust `String`. But
 * "concatenation" is three separate paths into that accumulator, and a fix to
 * one says nothing about the other two:
 *
 *   1. ARGUMENT   -- `"x" + lone + "y"`, the TAG_ARG recipe slot. The value is
 *                   a live `java/lang/String` on the heap, decoded per call.
 *   2. RECIPE     -- a folded lone-surrogate literal + n. javac folds the literal text INTO the
 *                   recipe string itself, which arrives as a Rust `&str`.
 *   3. CONSTANT   -- a TAG_CONST recipe slot, resolved from the constant pool
 *                   through `resolve_string_constant`.
 *
 * Only (1) round-trips through the heap. (2) and (3) are Rust text before
 * `execute_string_concat` is ever entered, so a units accumulator cannot help
 * them by itself -- the constant pool would have to carry units too (it can:
 * `ConstantPool::get_utf8_wide` exists for ANTLR's `_serializedATN`).
 *
 * Printing code units, not the string: a terminal renders every one of these
 * as the same replacement glyph, so `System.out.println(s)` cannot tell a pass
 * from a failure. Run on HotSpot first -- the point is the DIFFERENCE.
 */
public class StringConcatSurrogateProbe {

    static final char LONE = '\uD801';       // high surrogate, no low following
    static final char LONE_LO = '\uDC01';    // low surrogate, no high preceding

    static String units(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            if (i > 0) {
                sb.append(' ');
            }
            sb.append(String.format("%04X", (int) s.charAt(i)));
        }
        return sb.toString();
    }

    static void report(String label, String actual, String expected) {
        boolean ok = actual.equals(expected);
        System.out.println((ok ? "SAME " : "DIFF ") + label
                + "  len=" + actual.length()
                + "  units=[" + units(actual) + "]"
                + (ok ? "" : "  WANT=[" + units(expected) + "]"));
    }

    public static void main(String[] args) {
        // The control: never touches concatenation. This one already passed
        // before the fix, which is what made the bug easy to misattribute.
        String viaChars = new String(new char[] { 'x', LONE, 'y' });
        report("control new String(char[])", viaChars,
                new String(new char[] { 'x', LONE, 'y' }));

        // (1) ARGUMENT path.
        String lone = new String(new char[] { LONE });
        String arg = "x" + lone + "y";
        report("1 argument  \"x\"+lone+\"y\"", arg, viaChars);

        String loneLo = new String(new char[] { LONE_LO });
        report("1 argument  low surrogate", "x" + loneLo + "y",
                new String(new char[] { 'x', LONE_LO, 'y' }));

        // A surrogate PAIR split across two concat arguments must NOT be
        // recombined into one code point, and must not be mangled either.
        String hi = new String(new char[] { '\uD83D' });
        String lo = new String(new char[] { '\uDE00' });
        report("1 argument  split pair rejoins", hi + lo,
                new String(new char[] { '\uD83D', '\uDE00' }));

        // (2) RECIPE path: the literal is folded into the recipe by javac.
        int n = 7;
        String recipe = "x\uD801y" + n;
        report("2 recipe    \"x\\uD801y\"+n", recipe,
                new String(new char[] { 'x', LONE, 'y', '7' }));

        // (3) CONSTANT path: a non-foldable constant slot. Two runtime values
        // around a literal tends to emit the literal as a TAG_CONST constant.
        String a = new String(new char[] { 'a' });
        String b = new String(new char[] { 'b' });
        String constant = a + "\uD801" + b;
        report("3 constant  a+\"\\uD801\"+b", constant,
                new String(new char[] { 'a', LONE, 'b' }));

        // Well-formed text must be untouched by any of this.
        String greek = new String(new char[] { '\u03A3', '\u039F' });
        report("4 wellformed greek concat", "[" + greek + "]",
                new String(new char[] { '[', '\u03A3', '\u039F', ']' }));
        report("4 wellformed supplementary", "[" + "\uD83D\uDE00" + "]",
                new String(new char[] { '[', '\uD83D', '\uDE00', ']' }));

        // hashCode over a concat result: the fold is over code units, so a
        // U+FFFD substitution changes it. Ties this to the hashCode work.
        System.out.println("hash(arg)=" + arg.hashCode()
                + " hash(control)=" + viaChars.hashCode()
                + " equal=" + (arg.hashCode() == viaChars.hashCode()));

        System.out.println("CONCAT-SURROGATE-PROBE-DONE");
    }
}
