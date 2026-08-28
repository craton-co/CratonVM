import java.util.Arrays;

/** L2 — the `java/lang/StringBuilder`, `java/lang/StringBuffer` and
 *  `java/lang/AbstractStringBuilder` triples the `--jdk-only-report` marks
 *  `outcome=native-won`: a bridge native that ran in front of real JDK
 *  bytecode rather than losing the dispatch to it.
 *
 *  `register_string_builder_natives` registers 62 (name, descriptor) pairs and
 *  is called for all three class names, so the same Rust body serves three
 *  classes with three different contracts. That is the "shared surface" shape
 *  that has paid out three times in this campaign, and the three are NOT the
 *  same:
 *
 *    * `StringBuffer` is synchronized and caches `toStringCache`, which every
 *      mutator must invalidate. A missed invalidation is a stale `toString()`
 *      with no exception anywhere — the quietest failure in this surface, so
 *      section J asks it once per mutator.
 *    * `AbstractStringBuilder` is package-private and abstract; anything
 *      registered on it runs for BOTH subclasses.
 *    * the JDK's own `capacity()` growth is observable and specified.
 *
 *  METHOD: ask the contract EDGES. All 28 defects mined in the four families
 *  before this one were on edges — nulls, bounds, refusal types, constructor
 *  validation, naming special cases — and not one was a wrong answer to an
 *  ordinary call. A happy-path probe reports this family clean when it is not.
 *
 *  DETERMINISM: no identity hash, no address, no thread name, no timing, no
 *  hash-container iteration order is printed. Every non-ASCII character is
 *  escaped, so a lone surrogate survives the pipe to a diff. Exception rows
 *  print the class NAME only — messages are not specified across images.
 *
 *  Every row is produced by `p`, which catches Throwable itself, so one bad
 *  row cannot truncate the tail and hide the rest of the sweep behind a short
 *  file. The last line prints the row total for the same reason.
 */
public class StringBuilderShadowSweep {

    // ---------------------------------------------------------------- harness

    static int rows = 0;

    interface F {
        Object get() throws Throwable;
    }

    static final char[] HEX = "0123456789abcdef".toCharArray();

    /** Escapes without using a `StringBuilder`: the harness must not depend on
     *  the class under test, or a broken builder garbles every row instead of
     *  failing the rows that actually exercise it. */
    static String esc(String s) {
        if (s == null) {
            return "null";
        }
        char[] out = new char[s.length() * 6];
        int n = 0;
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) {
                out[n++] = '\\';
                out[n++] = 'u';
                out[n++] = HEX[(c >> 12) & 0xf];
                out[n++] = HEX[(c >> 8) & 0xf];
                out[n++] = HEX[(c >> 4) & 0xf];
                out[n++] = HEX[c & 0xf];
            } else {
                out[n++] = c;
            }
        }
        return new String(out, 0, n);
    }

    static void p(String tag, F f) {
        rows++;
        String v;
        try {
            Object o = f.get();
            v = String.valueOf(o);
        } catch (Throwable e) {
            v = "THREW " + e.getClass().getName();
        }
        System.out.print(esc(tag));
        System.out.print(" |");
        System.out.print(esc(v));
        System.out.println("|");
    }

    static void sect(String name, Runnable r) {
        try {
            r.run();
        } catch (Throwable e) {
            System.out.println("SECTION-ABORTED " + name + " " + e.getClass().getName());
        }
    }

    // The two supplementary-plane and lone-surrogate fixtures, spelled with
    // explicit escapes so the source itself carries no non-ASCII byte.
    static final String PAIR = "\uD83D\uDE00";   // U+1F600, one code point
    static final String LONE_HI = "\uD800";
    static final String LONE_LO = "\uDC00";
    static final char EURO = '\u20ac';           // the cheapest non-LATIN1 char

    // ------------------------------------------------------------- the facade
    //
    // One interface over both concrete builders so every edge below is asked
    // of `StringBuilder` and `StringBuffer` in the SAME order with the SAME
    // tags. The two classes share `register_string_builder_natives` but not
    // their contracts, and a sweep that asked only one of them would report
    // the other's surface untested.

    interface B {
        Object raw();

        Object append(String s);

        Object appendCs(CharSequence s);

        Object appendChars(char[] c);

        Object appendChars(char[] c, int off, int len);

        Object appendCs(CharSequence s, int start, int end);

        Object appendObj(Object o);

        Object appendBuf(StringBuffer s);

        Object append(int v);

        Object append(long v);

        Object append(float v);

        Object append(double v);

        Object append(boolean v);

        Object append(char v);

        Object appendCodePoint(int cp);

        Object insert(int i, String s);

        Object insert(int i, char c);

        Object insert(int i, int v);

        Object insertObj(int i, Object o);

        Object insert(int i, char[] c);

        Object insert(int i, char[] c, int off, int len);

        Object insert(int i, boolean v);

        Object insert(int i, long v);

        Object insert(int i, float v);

        Object insert(int i, double v);

        Object insertCs(int i, CharSequence s);

        Object insertCs(int i, CharSequence s, int start, int end);

        Object delete(int s, int e);

        Object deleteCharAt(int i);

        Object replace(int s, int e, String str);

        void setCharAt(int i, char c);

        void setLength(int n);

        char charAt(int i);

        int codePointAt(int i);

        int codePointBefore(int i);

        int codePointCount(int s, int e);

        int offsetByCodePoints(int i, int n);

        void getChars(int sb, int se, char[] dst, int db);

        Object reverse();

        int indexOf(String s);

        int indexOf(String s, int from);

        int lastIndexOf(String s);

        int lastIndexOf(String s, int from);

        String substring(int s);

        String substring(int s, int e);

        int capacity();

        void ensureCapacity(int n);

        void trimToSize();

        int length();

        boolean isEmpty();

        CharSequence subSequence(int s, int e);

        Object repeat(int cp, int count);

        Object repeatCs(CharSequence cs, int count);

        int[] chars();

        int[] codePoints();
    }

    static final class Sb implements B {
        final StringBuilder b;

        Sb(StringBuilder b) {
            this.b = b;
        }

        public Object raw() { return b; }
        public Object append(String s) { return b.append(s); }
        public Object appendCs(CharSequence s) { return b.append(s); }
        public Object appendChars(char[] c) { return b.append(c); }
        public Object appendChars(char[] c, int off, int len) { return b.append(c, off, len); }
        public Object appendCs(CharSequence s, int st, int e) { return b.append(s, st, e); }
        public Object appendObj(Object o) { return b.append(o); }
        public Object appendBuf(StringBuffer s) { return b.append(s); }
        public Object append(int v) { return b.append(v); }
        public Object append(long v) { return b.append(v); }
        public Object append(float v) { return b.append(v); }
        public Object append(double v) { return b.append(v); }
        public Object append(boolean v) { return b.append(v); }
        public Object append(char v) { return b.append(v); }
        public Object appendCodePoint(int cp) { return b.appendCodePoint(cp); }
        public Object insert(int i, String s) { return b.insert(i, s); }
        public Object insert(int i, char c) { return b.insert(i, c); }
        public Object insert(int i, int v) { return b.insert(i, v); }
        public Object insertObj(int i, Object o) { return b.insert(i, o); }
        public Object insert(int i, char[] c) { return b.insert(i, c); }
        public Object insert(int i, char[] c, int o, int l) { return b.insert(i, c, o, l); }
        public Object insert(int i, boolean v) { return b.insert(i, v); }
        public Object insert(int i, long v) { return b.insert(i, v); }
        public Object insert(int i, float v) { return b.insert(i, v); }
        public Object insert(int i, double v) { return b.insert(i, v); }
        public Object insertCs(int i, CharSequence s) { return b.insert(i, s); }
        public Object insertCs(int i, CharSequence s, int st, int e) { return b.insert(i, s, st, e); }
        public Object delete(int s, int e) { return b.delete(s, e); }
        public Object deleteCharAt(int i) { return b.deleteCharAt(i); }
        public Object replace(int s, int e, String str) { return b.replace(s, e, str); }
        public void setCharAt(int i, char c) { b.setCharAt(i, c); }
        public void setLength(int n) { b.setLength(n); }
        public char charAt(int i) { return b.charAt(i); }
        public int codePointAt(int i) { return b.codePointAt(i); }
        public int codePointBefore(int i) { return b.codePointBefore(i); }
        public int codePointCount(int s, int e) { return b.codePointCount(s, e); }
        public int offsetByCodePoints(int i, int n) { return b.offsetByCodePoints(i, n); }
        public void getChars(int sb, int se, char[] d, int db) { b.getChars(sb, se, d, db); }
        public Object reverse() { return b.reverse(); }
        public int indexOf(String s) { return b.indexOf(s); }
        public int indexOf(String s, int f) { return b.indexOf(s, f); }
        public int lastIndexOf(String s) { return b.lastIndexOf(s); }
        public int lastIndexOf(String s, int f) { return b.lastIndexOf(s, f); }
        public String substring(int s) { return b.substring(s); }
        public String substring(int s, int e) { return b.substring(s, e); }
        public int capacity() { return b.capacity(); }
        public void ensureCapacity(int n) { b.ensureCapacity(n); }
        public void trimToSize() { b.trimToSize(); }
        public int length() { return b.length(); }
        public boolean isEmpty() { return b.isEmpty(); }
        public CharSequence subSequence(int s, int e) { return b.subSequence(s, e); }
        public Object repeat(int cp, int c) { return b.repeat(cp, c); }
        public Object repeatCs(CharSequence cs, int c) { return b.repeat(cs, c); }
        public int[] chars() { return b.chars().toArray(); }
        public int[] codePoints() { return b.codePoints().toArray(); }
    }

    static final class Bf implements B {
        final StringBuffer b;

        Bf(StringBuffer b) {
            this.b = b;
        }

        public Object raw() { return b; }
        public Object append(String s) { return b.append(s); }
        public Object appendCs(CharSequence s) { return b.append(s); }
        public Object appendChars(char[] c) { return b.append(c); }
        public Object appendChars(char[] c, int off, int len) { return b.append(c, off, len); }
        public Object appendCs(CharSequence s, int st, int e) { return b.append(s, st, e); }
        public Object appendObj(Object o) { return b.append(o); }
        public Object appendBuf(StringBuffer s) { return b.append(s); }
        public Object append(int v) { return b.append(v); }
        public Object append(long v) { return b.append(v); }
        public Object append(float v) { return b.append(v); }
        public Object append(double v) { return b.append(v); }
        public Object append(boolean v) { return b.append(v); }
        public Object append(char v) { return b.append(v); }
        public Object appendCodePoint(int cp) { return b.appendCodePoint(cp); }
        public Object insert(int i, String s) { return b.insert(i, s); }
        public Object insert(int i, char c) { return b.insert(i, c); }
        public Object insert(int i, int v) { return b.insert(i, v); }
        public Object insertObj(int i, Object o) { return b.insert(i, o); }
        public Object insert(int i, char[] c) { return b.insert(i, c); }
        public Object insert(int i, char[] c, int o, int l) { return b.insert(i, c, o, l); }
        public Object insert(int i, boolean v) { return b.insert(i, v); }
        public Object insert(int i, long v) { return b.insert(i, v); }
        public Object insert(int i, float v) { return b.insert(i, v); }
        public Object insert(int i, double v) { return b.insert(i, v); }
        public Object insertCs(int i, CharSequence s) { return b.insert(i, s); }
        public Object insertCs(int i, CharSequence s, int st, int e) { return b.insert(i, s, st, e); }
        public Object delete(int s, int e) { return b.delete(s, e); }
        public Object deleteCharAt(int i) { return b.deleteCharAt(i); }
        public Object replace(int s, int e, String str) { return b.replace(s, e, str); }
        public void setCharAt(int i, char c) { b.setCharAt(i, c); }
        public void setLength(int n) { b.setLength(n); }
        public char charAt(int i) { return b.charAt(i); }
        public int codePointAt(int i) { return b.codePointAt(i); }
        public int codePointBefore(int i) { return b.codePointBefore(i); }
        public int codePointCount(int s, int e) { return b.codePointCount(s, e); }
        public int offsetByCodePoints(int i, int n) { return b.offsetByCodePoints(i, n); }
        public void getChars(int sb, int se, char[] d, int db) { b.getChars(sb, se, d, db); }
        public Object reverse() { return b.reverse(); }
        public int indexOf(String s) { return b.indexOf(s); }
        public int indexOf(String s, int f) { return b.indexOf(s, f); }
        public int lastIndexOf(String s) { return b.lastIndexOf(s); }
        public int lastIndexOf(String s, int f) { return b.lastIndexOf(s, f); }
        public String substring(int s) { return b.substring(s); }
        public String substring(int s, int e) { return b.substring(s, e); }
        public int capacity() { return b.capacity(); }
        public void ensureCapacity(int n) { b.ensureCapacity(n); }
        public void trimToSize() { b.trimToSize(); }
        public int length() { return b.length(); }
        public boolean isEmpty() { return b.isEmpty(); }
        public CharSequence subSequence(int s, int e) { return b.subSequence(s, e); }
        public Object repeat(int cp, int c) { return b.repeat(cp, c); }
        public Object repeatCs(CharSequence cs, int c) { return b.repeat(cs, c); }
        public int[] chars() { return b.chars().toArray(); }
        public int[] codePoints() { return b.codePoints().toArray(); }
    }

    interface Mk {
        B of(String initial);
    }

    static B sb(String s) { return new Sb(new StringBuilder(s)); }

    static B bf(String s) { return new Bf(new StringBuffer(s)); }

    /** A CharSequence that is NOT a String / builder, so the shim cannot take a
     *  fast path keyed on the concrete type. `toString()` deliberately answers
     *  something DIFFERENT from `charAt`, which separates a shim that reads the
     *  sequence the way `AbstractStringBuilder.append(CharSequence)` does
     *  (charAt, per the JDK's own body) from one that calls `toString()`. */
    static final class Seq implements CharSequence {
        final String s;
        final String lie;

        Seq(String s, String lie) {
            this.s = s;
            this.lie = lie;
        }

        public int length() { return s.length(); }

        public char charAt(int i) { return s.charAt(i); }

        public CharSequence subSequence(int a, int b) { return s.subSequence(a, b); }

        public String toString() { return lie; }
    }

    // ------------------------------------------------------- A. construction

    static void construction(String pre, boolean buffer) {
        if (buffer) {
            p(pre + "new()  capacity", () -> new StringBuffer().capacity());
            p(pre + "new()  length", () -> new StringBuffer().length());
            p(pre + "new()  toString", () -> new StringBuffer().toString());
            p(pre + "new(0) capacity", () -> new StringBuffer(0).capacity());
            p(pre + "new(0) length", () -> new StringBuffer(0).length());
            p(pre + "new(-1)", () -> new StringBuffer(-1));
            p(pre + "new(-7)", () -> new StringBuffer(-7));
            p(pre + "new(1 shl 20) capacity", () -> new StringBuffer(1 << 20).capacity());
            p(pre + "new(str) capacity", () -> new StringBuffer("abc").capacity());
            p(pre + "new(str) toString", () -> new StringBuffer("abc").toString());
            p(pre + "new(empty str) capacity", () -> new StringBuffer("").capacity());
            p(pre + "new((String)null)", () -> new StringBuffer((String) null).length());
            p(pre + "new((CharSequence)null)", () -> new StringBuffer((CharSequence) null).length());
            p(pre + "new(cs) reads charAt not toString",
                    () -> new StringBuffer(new Seq("abc", "LIE")).toString());
            p(pre + "new(cs) capacity", () -> new StringBuffer(new Seq("abc", "LIE")).capacity());
            p(pre + "new(builder)", () -> new StringBuffer(new StringBuilder("xy")).toString());
            p(pre + "new(lone hi) length", () -> new StringBuffer(LONE_HI).length());
            p(pre + "new(lone hi) charAt0", () -> (int) new StringBuffer(LONE_HI).charAt(0));
            p(pre + "new(pair) contentEquals",
                    () -> PAIR.contentEquals(new StringBuffer(PAIR)));
        } else {
            p(pre + "new()  capacity", () -> new StringBuilder().capacity());
            p(pre + "new()  length", () -> new StringBuilder().length());
            p(pre + "new()  toString", () -> new StringBuilder().toString());
            p(pre + "new(0) capacity", () -> new StringBuilder(0).capacity());
            p(pre + "new(0) length", () -> new StringBuilder(0).length());
            p(pre + "new(-1)", () -> new StringBuilder(-1));
            p(pre + "new(-7)", () -> new StringBuilder(-7));
            p(pre + "new(1 shl 20) capacity", () -> new StringBuilder(1 << 20).capacity());
            p(pre + "new(str) capacity", () -> new StringBuilder("abc").capacity());
            p(pre + "new(str) toString", () -> new StringBuilder("abc").toString());
            p(pre + "new(empty str) capacity", () -> new StringBuilder("").capacity());
            p(pre + "new((String)null)", () -> new StringBuilder((String) null).length());
            p(pre + "new((CharSequence)null)", () -> new StringBuilder((CharSequence) null).length());
            p(pre + "new(cs) reads charAt not toString",
                    () -> new StringBuilder(new Seq("abc", "LIE")).toString());
            p(pre + "new(cs) capacity", () -> new StringBuilder(new Seq("abc", "LIE")).capacity());
            p(pre + "new(builder)", () -> new StringBuilder(new StringBuffer("xy")).toString());
            p(pre + "new(lone hi) length", () -> new StringBuilder(LONE_HI).length());
            p(pre + "new(lone hi) charAt0", () -> (int) new StringBuilder(LONE_HI).charAt(0));
            p(pre + "new(pair) contentEquals",
                    () -> PAIR.contentEquals(new StringBuilder(PAIR)));
        }
    }

    // ------------------------------------------------------------- B. append

    static void appends(String pre, Mk mk) {
        p(pre + "append(String) returns this",
                () -> { B b = mk.of("a"); return b.append("b") == b.raw(); });
        p(pre + "append(String)", () -> mk.of("a").append("bc"));
        p(pre + "append((String)null)", () -> mk.of("a").append(null));
        p(pre + "append((CharSequence)null)", () -> mk.of("a").appendCs(null));
        p(pre + "append((Object)null)", () -> mk.of("a").appendObj(null));
        p(pre + "append((StringBuffer)null)", () -> mk.of("a").appendBuf(null));
        p(pre + "append((char[])null)", () -> mk.of("a").appendChars(null));
        p(pre + "append((char[])null,0,0)", () -> mk.of("a").appendChars(null, 0, 0));
        p(pre + "append(empty String) length", () -> mk.of("a").append("").toString().length());
        p(pre + "append(char[])", () -> mk.of("a").appendChars(new char[] { 'x', 'y' }));
        p(pre + "append(char[],1,1)",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 1, 1));
        p(pre + "append(char[],0,3) exact",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 0, 3));
        p(pre + "append(char[],-1,1)",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, -1, 1));
        p(pre + "append(char[],0,-1)",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 0, -1));
        p(pre + "append(char[],2,2)",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 2, 2));
        p(pre + "append(char[],3,0) at end",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 3, 0));
        // off + len overflows to a NEGATIVE int: the case a check written
        // `off + len > b.length` gets wrong while looking right.
        p(pre + "append(char[],1,MAX_VALUE) overflow",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, 1, Integer.MAX_VALUE));
        p(pre + "append(char[],MAX_VALUE,1) overflow",
                () -> mk.of("a").appendChars(new char[] { 'x', 'y', 'z' }, Integer.MAX_VALUE, 1));
        p(pre + "append(CharSequence)", () -> mk.of("a").appendCs(new Seq("bc", "LIE")));
        p(pre + "append(CharSequence,1,2)", () -> mk.of("a").appendCs(new Seq("bcd", "LIE"), 1, 2));
        // The JDK applies the window to the four characters of the literal
        // "null" when the sequence is null — it does NOT throw NPE.
        p(pre + "append((CharSequence)null,1,3)", () -> mk.of("a").appendCs(null, 1, 3));
        p(pre + "append((CharSequence)null,0,4)", () -> mk.of("a").appendCs(null, 0, 4));
        p(pre + "append((CharSequence)null,0,5)", () -> mk.of("a").appendCs(null, 0, 5));
        p(pre + "append(CharSequence,2,1) reversed",
                () -> mk.of("a").appendCs(new Seq("bcd", "LIE"), 2, 1));
        p(pre + "append(CharSequence,-1,1)", () -> mk.of("a").appendCs(new Seq("bcd", "LIE"), -1, 1));
        p(pre + "append(CharSequence,1,9)", () -> mk.of("a").appendCs(new Seq("bcd", "LIE"), 1, 9));
        p(pre + "append(CharSequence,3,3) empty at end",
                () -> mk.of("a").appendCs(new Seq("bcd", "LIE"), 3, 3));
        p(pre + "append(Object) uses toString",
                () -> mk.of("a").appendObj(new Seq("bcd", "LIE")));
        p(pre + "append(StringBuffer)", () -> mk.of("a").appendBuf(new StringBuffer("Q")));
        p(pre + "append(int MIN)", () -> mk.of("").append(Integer.MIN_VALUE));
        p(pre + "append(int 0)", () -> mk.of("").append(0));
        p(pre + "append(long MIN)", () -> mk.of("").append(Long.MIN_VALUE));
        p(pre + "append(boolean)", () -> mk.of("").append(true).toString() + mk.of("").append(false));
        p(pre + "append(char)", () -> mk.of("").append('Z'));
        p(pre + "append(char 0)", () -> mk.of("").append('\0'));
        p(pre + "append(double 0.1)", () -> mk.of("").append(0.1d));
        p(pre + "append(double -0.0)", () -> mk.of("").append(-0.0d));
        p(pre + "append(double NaN)", () -> mk.of("").append(Double.NaN));
        p(pre + "append(double Inf)", () -> mk.of("").append(Double.NEGATIVE_INFINITY));
        p(pre + "append(double 1e20)", () -> mk.of("").append(1e20d));
        p(pre + "append(double 1e-7)", () -> mk.of("").append(1e-7d));
        p(pre + "append(float 1.1)", () -> mk.of("").append(1.1f));
        p(pre + "append(float -0.0)", () -> mk.of("").append(-0.0f));
        p(pre + "append(float NaN)", () -> mk.of("").append(Float.NaN));
        p(pre + "append lone high then low is a pair", () -> {
            B b = mk.of("");
            b.append(LONE_HI);
            b.append(LONE_LO);
            return b.raw().toString().codePointAt(0);
        });
        p(pre + "appendCodePoint(BMP)", () -> mk.of("").appendCodePoint(0x41));
        p(pre + "appendCodePoint(supplementary) length",
                () -> mk.of("").appendCodePoint(0x1F600).toString().length());
        p(pre + "appendCodePoint(supplementary)", () -> mk.of("").appendCodePoint(0x1F600));
        p(pre + "appendCodePoint(-1)", () -> mk.of("").appendCodePoint(-1));
        p(pre + "appendCodePoint(0x110000)", () -> mk.of("").appendCodePoint(0x110000));
        p(pre + "appendCodePoint(MAX_VALUE)", () -> mk.of("").appendCodePoint(Integer.MAX_VALUE));
        // A lone surrogate IS a valid code point; the JDK appends it.
        p(pre + "appendCodePoint(0xD800) length",
                () -> mk.of("").appendCodePoint(0xD800).toString().length());
        p(pre + "appendCodePoint(0xD800)", () -> mk.of("").appendCodePoint(0xD800));
        p(pre + "appendCodePoint(0x10FFFF) length",
                () -> mk.of("").appendCodePoint(0x10FFFF).toString().length());
        p(pre + "appendCodePoint(0)", () -> mk.of("").appendCodePoint(0));
    }

    // ------------------------------------------------------------- C. insert

    static void inserts(String pre, Mk mk) {
        p(pre + "insert(0,String)", () -> mk.of("abc").insert(0, "X"));
        p(pre + "insert(len,String)", () -> mk.of("abc").insert(3, "X"));
        p(pre + "insert(len+1,String)", () -> mk.of("abc").insert(4, "X"));
        p(pre + "insert(-1,String)", () -> mk.of("abc").insert(-1, "X"));
        p(pre + "insert(1,(String)null)", () -> mk.of("abc").insert(1, (String) null));
        p(pre + "insert(9,(String)null) bad offset first",
                () -> mk.of("abc").insert(9, (String) null));
        p(pre + "insert(1,char)", () -> mk.of("abc").insert(1, 'X'));
        p(pre + "insert(1,int)", () -> mk.of("abc").insert(1, -5));
        p(pre + "insert(1,Object null)", () -> mk.of("abc").insertObj(1, null));
        p(pre + "insert(1,Object)", () -> mk.of("abc").insertObj(1, new Seq("q", "LIE")));
        p(pre + "insert(1,char[])", () -> mk.of("abc").insert(1, new char[] { 'x', 'y' }));
        p(pre + "insert(1,(char[])null)", () -> mk.of("abc").insert(1, (char[]) null));
        p(pre + "insert(1,char[],0,1)",
                () -> mk.of("abc").insert(1, new char[] { 'x', 'y' }, 0, 1));
        p(pre + "insert(1,char[],-1,1)",
                () -> mk.of("abc").insert(1, new char[] { 'x', 'y' }, -1, 1));
        p(pre + "insert(1,char[],0,9)",
                () -> mk.of("abc").insert(1, new char[] { 'x', 'y' }, 0, 9));
        p(pre + "insert(1,char[],1,MAX_VALUE) overflow",
                () -> mk.of("abc").insert(1, new char[] { 'x', 'y' }, 1, Integer.MAX_VALUE));
        p(pre + "insert(9,char[],0,1) bad offset first",
                () -> mk.of("abc").insert(9, new char[] { 'x', 'y' }, 0, 1));
        p(pre + "insert(1,(char[])null,0,1)",
                () -> mk.of("abc").insert(1, (char[]) null, 0, 1));
        p(pre + "insert(1,boolean)", () -> mk.of("abc").insert(1, true));
        p(pre + "insert(1,long)", () -> mk.of("abc").insert(1, Long.MIN_VALUE));
        p(pre + "insert(1,float)", () -> mk.of("abc").insert(1, 1.5f));
        p(pre + "insert(1,double)", () -> mk.of("abc").insert(1, -0.0d));
        p(pre + "insert(1,CharSequence)", () -> mk.of("abc").insertCs(1, new Seq("q", "LIE")));
        p(pre + "insert(1,(CharSequence)null)", () -> mk.of("abc").insertCs(1, null));
        p(pre + "insert(1,CharSequence,0,1)",
                () -> mk.of("abc").insertCs(1, new Seq("qr", "LIE"), 0, 1));
        p(pre + "insert(1,(CharSequence)null,1,3)", () -> mk.of("abc").insertCs(1, null, 1, 3));
        p(pre + "insert(1,CharSequence,2,1) reversed",
                () -> mk.of("abc").insertCs(1, new Seq("qr", "LIE"), 2, 1));
        p(pre + "insert(1,CharSequence,0,9)",
                () -> mk.of("abc").insertCs(1, new Seq("qr", "LIE"), 0, 9));
        p(pre + "insert(9,CharSequence,0,1) bad offset",
                () -> mk.of("abc").insertCs(9, new Seq("qr", "LIE"), 0, 1));
        p(pre + "insert returns this",
                () -> { B b = mk.of("abc"); return b.insert(0, "z") == b.raw(); });
    }

    // ------------------------------- D. delete / replace / setLength / setCharAt

    static void mutators(String pre, Mk mk) {
        p(pre + "delete(1,2)", () -> mk.of("abcde").delete(1, 2));
        p(pre + "delete(1,99) clamps end", () -> mk.of("abcde").delete(1, 99));
        p(pre + "delete(0,0) empty window", () -> mk.of("abcde").delete(0, 0));
        p(pre + "delete(5,5) at end", () -> mk.of("abcde").delete(5, 5));
        p(pre + "delete(6,6) past end", () -> mk.of("abcde").delete(6, 6));
        p(pre + "delete(3,1) reversed", () -> mk.of("abcde").delete(3, 1));
        p(pre + "delete(-1,2)", () -> mk.of("abcde").delete(-1, 2));
        p(pre + "delete(0,99) whole", () -> mk.of("abcde").delete(0, 99).toString().length());
        p(pre + "deleteCharAt(0)", () -> mk.of("abcde").deleteCharAt(0));
        p(pre + "deleteCharAt(4) last", () -> mk.of("abcde").deleteCharAt(4));
        p(pre + "deleteCharAt(5) at length", () -> mk.of("abcde").deleteCharAt(5));
        p(pre + "deleteCharAt(-1)", () -> mk.of("abcde").deleteCharAt(-1));
        p(pre + "replace(1,2,XY)", () -> mk.of("abcde").replace(1, 2, "XY"));
        p(pre + "replace(0,99,Q) clamps", () -> mk.of("abcde").replace(0, 99, "Q"));
        p(pre + "replace(5,5,Q) at end", () -> mk.of("abcde").replace(5, 5, "Q"));
        p(pre + "replace(6,7,Q) past end", () -> mk.of("abcde").replace(6, 7, "Q"));
        p(pre + "replace(3,1,Q) reversed", () -> mk.of("abcde").replace(3, 1, "Q"));
        p(pre + "replace(-1,2,Q)", () -> mk.of("abcde").replace(-1, 2, "Q"));
        p(pre + "replace(1,2,null)", () -> mk.of("abcde").replace(1, 2, null));
        p(pre + "replace(9,9,null) bad range first", () -> mk.of("abcde").replace(9, 9, null));
        p(pre + "replace with longer grows",
                () -> mk.of("abcde").replace(0, 5, "0123456789012345678901234567890"));
        p(pre + "setLength(0)", () -> { B b = mk.of("abcde"); b.setLength(0); return b.raw() + "/" + b.length(); });
        p(pre + "setLength(2)", () -> { B b = mk.of("abcde"); b.setLength(2); return b.raw(); });
        p(pre + "setLength(8) pads NUL", () -> { B b = mk.of("abcde"); b.setLength(8); return b.raw(); });
        p(pre + "setLength(8) length", () -> { B b = mk.of("abcde"); b.setLength(8); return b.length(); });
        p(pre + "setLength(-1)", () -> { B b = mk.of("abcde"); b.setLength(-1); return b.raw(); });
        p(pre + "setLength(5) identity", () -> { B b = mk.of("abcde"); b.setLength(5); return b.raw(); });
        p(pre + "setLength then append",
                () -> { B b = mk.of("abcde"); b.setLength(2); return b.append("Z"); });
        p(pre + "setLength(0) then append",
                () -> { B b = mk.of("abcde"); b.setLength(0); return b.append("Z"); });
        p(pre + "setCharAt(0)", () -> { B b = mk.of("abcde"); b.setCharAt(0, 'Z'); return b.raw(); });
        p(pre + "setCharAt(4) last", () -> { B b = mk.of("abcde"); b.setCharAt(4, 'Z'); return b.raw(); });
        p(pre + "setCharAt(5) at length", () -> { B b = mk.of("abcde"); b.setCharAt(5, 'Z'); return b.raw(); });
        p(pre + "setCharAt(-1)", () -> { B b = mk.of("abcde"); b.setCharAt(-1, 'Z'); return b.raw(); });
        p(pre + "setCharAt to non-latin1",
                () -> { B b = mk.of("abcde"); b.setCharAt(0, EURO); return b.raw(); });
    }

    // ----------------------------------------- E. readers and code-point views

    static void readers(String pre, Mk mk) {
        p(pre + "charAt(0)", () -> mk.of("abc").charAt(0));
        p(pre + "charAt(2)", () -> mk.of("abc").charAt(2));
        p(pre + "charAt(3)", () -> mk.of("abc").charAt(3));
        p(pre + "charAt(-1)", () -> mk.of("abc").charAt(-1));
        p(pre + "charAt on empty", () -> mk.of("").charAt(0));
        p(pre + "length", () -> mk.of("abc").length());
        p(pre + "isEmpty true", () -> mk.of("").isEmpty());
        p(pre + "isEmpty false", () -> mk.of("a").isEmpty());
        p(pre + "codePointAt pair", () -> mk.of(PAIR).codePointAt(0));
        p(pre + "codePointAt pair low half", () -> mk.of(PAIR).codePointAt(1));
        p(pre + "codePointAt lone hi", () -> mk.of(LONE_HI).codePointAt(0));
        p(pre + "codePointAt(-1)", () -> mk.of("abc").codePointAt(-1));
        p(pre + "codePointAt(len)", () -> mk.of("abc").codePointAt(3));
        p(pre + "codePointBefore(len) pair", () -> mk.of(PAIR).codePointBefore(2));
        p(pre + "codePointBefore(1) pair", () -> mk.of(PAIR).codePointBefore(1));
        p(pre + "codePointBefore(0)", () -> mk.of("abc").codePointBefore(0));
        p(pre + "codePointBefore(len+1)", () -> mk.of("abc").codePointBefore(4));
        p(pre + "codePointCount(0,len) pair", () -> mk.of("a" + PAIR + "b").codePointCount(0, 4));
        p(pre + "codePointCount(1,3) pair", () -> mk.of("a" + PAIR + "b").codePointCount(1, 3));
        p(pre + "codePointCount(1,2) split pair", () -> mk.of("a" + PAIR + "b").codePointCount(1, 2));
        p(pre + "codePointCount(-1,1)", () -> mk.of("abc").codePointCount(-1, 1));
        p(pre + "codePointCount(0,9)", () -> mk.of("abc").codePointCount(0, 9));
        p(pre + "codePointCount(2,1) reversed", () -> mk.of("abc").codePointCount(2, 1));
        p(pre + "offsetByCodePoints(0,1) pair", () -> mk.of("a" + PAIR + "b").offsetByCodePoints(0, 1));
        p(pre + "offsetByCodePoints(1,1) pair", () -> mk.of("a" + PAIR + "b").offsetByCodePoints(1, 1));
        p(pre + "offsetByCodePoints(4,-1) pair", () -> mk.of("a" + PAIR + "b").offsetByCodePoints(4, -1));
        p(pre + "offsetByCodePoints(0,9)", () -> mk.of("abc").offsetByCodePoints(0, 9));
        p(pre + "offsetByCodePoints(-1,0)", () -> mk.of("abc").offsetByCodePoints(-1, 0));
        p(pre + "chars of pair", () -> Arrays.toString(mk.of(PAIR).chars()));
        p(pre + "chars of lone hi", () -> Arrays.toString(mk.of(LONE_HI).chars()));
        p(pre + "codePoints of pair", () -> Arrays.toString(mk.of(PAIR).codePoints()));
        p(pre + "codePoints of lone hi", () -> Arrays.toString(mk.of(LONE_HI).codePoints()));
        p(pre + "codePoints of lone lo then a",
                () -> Arrays.toString(mk.of(LONE_LO + "a").codePoints()));
        p(pre + "codePoints of a+pair+b", () -> Arrays.toString(mk.of("a" + PAIR + "b").codePoints()));
        p(pre + "chars of empty", () -> Arrays.toString(mk.of("").chars()));
        p(pre + "subSequence(1,2)", () -> mk.of("abc").subSequence(1, 2));
        p(pre + "subSequence(0,0)", () -> "[" + mk.of("abc").subSequence(0, 0) + "]");
        p(pre + "subSequence(2,1)", () -> mk.of("abc").subSequence(2, 1));
        p(pre + "subSequence(0,9)", () -> mk.of("abc").subSequence(0, 9));
        p(pre + "substring(1)", () -> mk.of("abc").substring(1));
        p(pre + "substring(3) at end", () -> "[" + mk.of("abc").substring(3) + "]");
        p(pre + "substring(4)", () -> mk.of("abc").substring(4));
        p(pre + "substring(-1)", () -> mk.of("abc").substring(-1));
        p(pre + "substring(1,2)", () -> mk.of("abc").substring(1, 2));
        p(pre + "substring(2,1)", () -> mk.of("abc").substring(2, 1));
        p(pre + "substring(0,9)", () -> mk.of("abc").substring(0, 9));
        p(pre + "substring(3,3) at end", () -> "[" + mk.of("abc").substring(3, 3) + "]");
    }

    // ------------------------------------------------------------ F. getChars

    static void getChars(String pre, Mk mk) {
        p(pre + "getChars valid", () -> {
            char[] d = new char[5];
            Arrays.fill(d, '.');
            mk.of("abc").getChars(0, 3, d, 1);
            return new String(d);
        });
        p(pre + "getChars empty window", () -> {
            char[] d = new char[3];
            Arrays.fill(d, '.');
            mk.of("abc").getChars(1, 1, d, 3);
            return new String(d);
        });
        p(pre + "getChars null dst", () -> { mk.of("abc").getChars(0, 3, null, 0); return "no-throw"; });
        p(pre + "getChars null dst empty window",
                () -> { mk.of("abc").getChars(1, 1, null, 0); return "no-throw"; });
        p(pre + "getChars srcBegin -1",
                () -> { mk.of("abc").getChars(-1, 3, new char[9], 0); return "no-throw"; });
        p(pre + "getChars srcEnd 9",
                () -> { mk.of("abc").getChars(0, 9, new char[9], 0); return "no-throw"; });
        p(pre + "getChars srcBegin > srcEnd",
                () -> { mk.of("abc").getChars(2, 1, new char[9], 0); return "no-throw"; });
        p(pre + "getChars dstBegin -1",
                () -> { mk.of("abc").getChars(0, 3, new char[9], -1); return "no-throw"; });
        p(pre + "getChars dst too small",
                () -> { mk.of("abc").getChars(0, 3, new char[2], 0); return "no-throw"; });
        p(pre + "getChars dst overrun",
                () -> { mk.of("abc").getChars(0, 3, new char[4], 2); return "no-throw"; });
        p(pre + "getChars bad src beats null dst",
                () -> { mk.of("abc").getChars(2, 1, null, 0); return "no-throw"; });
    }

    // ------------------------------------------------------------- G. reverse

    static void reverse(String pre, Mk mk) {
        p(pre + "reverse abc", () -> mk.of("abc").reverse());
        p(pre + "reverse empty", () -> "[" + mk.of("").reverse() + "]");
        p(pre + "reverse single", () -> mk.of("a").reverse());
        // The JDK preserves valid surrogate PAIRS rather than reversing the
        // two code units that spell them.
        p(pre + "reverse pair", () -> mk.of(PAIR).reverse().toString().codePointAt(0));
        p(pre + "reverse a+pair+b", () -> mk.of("a" + PAIR + "b").reverse());
        p(pre + "reverse lone hi then lo", () -> mk.of(LONE_HI + LONE_LO).reverse());
        p(pre + "reverse lone lo then hi", () -> mk.of(LONE_LO + LONE_HI).reverse());
        p(pre + "reverse two pairs",
                () -> mk.of(PAIR + PAIR).reverse().toString().codePointCount(0, 4));
        p(pre + "reverse returns this",
                () -> { B b = mk.of("abc"); return b.reverse() == b.raw(); });
        p(pre + "reverse twice is identity", () -> mk.of("a" + PAIR + "b").reverse().toString()
                .contentEquals(new StringBuilder("a" + PAIR + "b").reverse().reverse().reverse()));
    }

    // ------------------------------------------------------- H. search family

    static void search(String pre, Mk mk) {
        p(pre + "indexOf found", () -> mk.of("abcabc").indexOf("bc"));
        p(pre + "indexOf missing", () -> mk.of("abcabc").indexOf("zz"));
        p(pre + "indexOf empty", () -> mk.of("abcabc").indexOf(""));
        p(pre + "indexOf empty from 99", () -> mk.of("abcabc").indexOf("", 99));
        p(pre + "indexOf empty from -1", () -> mk.of("abcabc").indexOf("", -1));
        p(pre + "indexOf from 2", () -> mk.of("abcabc").indexOf("bc", 2));
        p(pre + "indexOf from -1", () -> mk.of("abcabc").indexOf("bc", -1));
        p(pre + "indexOf from 99", () -> mk.of("abcabc").indexOf("bc", 99));
        p(pre + "indexOf null", () -> mk.of("abc").indexOf(null));
        p(pre + "indexOf null,0", () -> mk.of("abc").indexOf(null, 0));
        p(pre + "lastIndexOf found", () -> mk.of("abcabc").lastIndexOf("bc"));
        p(pre + "lastIndexOf empty", () -> mk.of("abcabc").lastIndexOf(""));
        p(pre + "lastIndexOf empty from 2", () -> mk.of("abcabc").lastIndexOf("", 2));
        p(pre + "lastIndexOf empty from 99", () -> mk.of("abcabc").lastIndexOf("", 99));
        p(pre + "lastIndexOf from 2", () -> mk.of("abcabc").lastIndexOf("bc", 2));
        p(pre + "lastIndexOf from -1", () -> mk.of("abcabc").lastIndexOf("bc", -1));
        p(pre + "lastIndexOf null", () -> mk.of("abc").lastIndexOf(null));
        p(pre + "indexOf on empty builder", () -> mk.of("").indexOf(""));
    }

    // ------------------------------------------------- I. capacity and growth

    static void capacity(String pre, Mk mk) {
        p(pre + "capacity of new(str)", () -> mk.of("abc").capacity());
        p(pre + "ensureCapacity(-1) no-op",
                () -> { B b = mk.of("abc"); b.ensureCapacity(-1); return b.capacity(); });
        p(pre + "ensureCapacity(0) no-op",
                () -> { B b = mk.of("abc"); b.ensureCapacity(0); return b.capacity(); });
        p(pre + "ensureCapacity(smaller) no-op",
                () -> { B b = mk.of("abc"); b.ensureCapacity(5); return b.capacity(); });
        // 2*old+2 = 40 for old=19, which is >= 25, so the doubling rule wins.
        p(pre + "ensureCapacity(25) doubling rule",
                () -> { B b = mk.of("abc"); b.ensureCapacity(25); return b.capacity(); });
        // 2*old+2 = 40 < 100, so the request wins.
        p(pre + "ensureCapacity(100) request wins",
                () -> { B b = mk.of("abc"); b.ensureCapacity(100); return b.capacity(); });
        p(pre + "ensureCapacity keeps content",
                () -> { B b = mk.of("abc"); b.ensureCapacity(100); return b.raw(); });
        p(pre + "trimToSize capacity",
                () -> { B b = mk.of("abc"); b.trimToSize(); return b.capacity(); });
        p(pre + "trimToSize keeps content",
                () -> { B b = mk.of("abc"); b.trimToSize(); return b.raw(); });
        p(pre + "trimToSize then append",
                () -> { B b = mk.of("abc"); b.trimToSize(); return b.append("XY"); });
        p(pre + "trimToSize on empty capacity",
                () -> { B b = mk.of(""); b.trimToSize(); return b.capacity(); });
        p(pre + "trimToSize twice",
                () -> { B b = mk.of("abc"); b.trimToSize(); b.trimToSize(); return b.capacity(); });
        p(pre + "capacity after growing append", () -> {
            B b = mk.of("");
            for (int i = 0; i < 17; i++) {
                b.append('x');
            }
            return b.capacity();
        });
        p(pre + "content after growing append", () -> {
            B b = mk.of("");
            for (int i = 0; i < 40; i++) {
                b.append('x');
            }
            return b.raw().toString().length();
        });
        p(pre + "capacity after 40 appends", () -> {
            B b = mk.of("");
            for (int i = 0; i < 40; i++) {
                b.append('x');
            }
            return b.capacity();
        });
        p(pre + "capacity unchanged by a non-latin1 append",
                () -> { B b = mk.of("abc"); b.append(EURO); return b.capacity(); });
        p(pre + "non-latin1 content survives",
                () -> { B b = mk.of("abc"); b.append(EURO); return b.raw(); });
        p(pre + "setLength(0) does not shrink capacity",
                () -> { B b = mk.of("abc"); b.setLength(0); return b.capacity(); });
        p(pre + "setLength(bigger than capacity) grows",
                () -> { B b = mk.of("abc"); b.setLength(100); return b.capacity(); });
    }

    // ------------------------------------------------------------ J. repeat

    static void repeat(String pre, Mk mk) {
        p(pre + "repeat(char,3)", () -> mk.of("a").repeat('x', 3));
        p(pre + "repeat(char,0)", () -> mk.of("a").repeat('x', 0));
        p(pre + "repeat(char,-1)", () -> mk.of("a").repeat('x', -1));
        p(pre + "repeat(supplementary,2) length",
                () -> mk.of("").repeat(0x1F600, 2).toString().length());
        p(pre + "repeat(-1,2) invalid code point", () -> mk.of("a").repeat(-1, 2));
        p(pre + "repeat(0x110000,2)", () -> mk.of("a").repeat(0x110000, 2));
        p(pre + "repeat(CharSequence,2)", () -> mk.of("a").repeatCs(new Seq("bc", "LIE"), 2));
        p(pre + "repeat(CharSequence,0)", () -> mk.of("a").repeatCs(new Seq("bc", "LIE"), 0));
        p(pre + "repeat(CharSequence,-1)", () -> mk.of("a").repeatCs(new Seq("bc", "LIE"), -1));
        p(pre + "repeat((CharSequence)null,2)", () -> mk.of("a").repeatCs(null, 2));
        p(pre + "repeat((CharSequence)null,0)", () -> mk.of("a").repeatCs(null, 0));
        p(pre + "repeat((CharSequence)null,-1)", () -> mk.of("a").repeatCs(null, -1));
        p(pre + "repeat(empty,5)", () -> mk.of("a").repeatCs("", 5));
        p(pre + "repeat(String,3)", () -> mk.of("a").repeatCs("bc", 3));
        p(pre + "repeat capacity grows",
                () -> { B b = mk.of("a"); b.repeatCs("bc", 30); return b.length(); });
        p(pre + "repeat returns this",
                () -> { B b = mk.of("a"); return b.repeatCs("z", 2) == b.raw(); });
    }

    // ---------------------------------------- K. StringBuffer's toStringCache
    //
    // `StringBuffer` caches the last `toString()` in `toStringCache` and EVERY
    // mutator must null it. A missed invalidation is a stale `toString()` with
    // no exception anywhere — the quietest failure shape in this surface, and
    // one that only shows if the cache is warmed FIRST. Each row below calls
    // `toString()` once to warm it, mutates, and reads it again.

    static StringBuffer warm(String s) {
        StringBuffer b = new StringBuffer(s);
        b.toString();
        return b;
    }

    static void cache() {
        p("buf cache append(String)", () -> warm("abc").append("Z"));
        p("buf cache append(char)", () -> warm("abc").append('Z'));
        p("buf cache append(int)", () -> warm("abc").append(7));
        p("buf cache append(long)", () -> warm("abc").append(7L));
        p("buf cache append(boolean)", () -> warm("abc").append(true));
        p("buf cache append(double)", () -> warm("abc").append(0.5d));
        p("buf cache append(float)", () -> warm("abc").append(0.5f));
        p("buf cache append(char[])", () -> warm("abc").append(new char[] { 'Z' }));
        p("buf cache append(char[],0,1)", () -> warm("abc").append(new char[] { 'Z' }, 0, 1));
        p("buf cache append(Object)", () -> warm("abc").append((Object) "Z"));
        p("buf cache append(CharSequence)", () -> warm("abc").append((CharSequence) "Z"));
        p("buf cache append(CharSequence,0,1)", () -> warm("abc").append((CharSequence) "Z", 0, 1));
        p("buf cache append(StringBuffer)", () -> warm("abc").append(new StringBuffer("Z")));
        p("buf cache append(null)", () -> warm("abc").append((String) null));
        p("buf cache appendCodePoint", () -> warm("abc").appendCodePoint(0x5a));
        p("buf cache insert(String)", () -> warm("abc").insert(0, "Z"));
        p("buf cache insert(char)", () -> warm("abc").insert(0, 'Z'));
        p("buf cache insert(int)", () -> warm("abc").insert(0, 7));
        p("buf cache insert(long)", () -> warm("abc").insert(0, 7L));
        p("buf cache insert(boolean)", () -> warm("abc").insert(0, true));
        p("buf cache insert(float)", () -> warm("abc").insert(0, 0.5f));
        p("buf cache insert(double)", () -> warm("abc").insert(0, 0.5d));
        p("buf cache insert(Object)", () -> warm("abc").insert(0, (Object) "Z"));
        p("buf cache insert(char[])", () -> warm("abc").insert(0, new char[] { 'Z' }));
        p("buf cache insert(char[],0,1)", () -> warm("abc").insert(0, new char[] { 'Z' }, 0, 1));
        p("buf cache insert(CharSequence)", () -> warm("abc").insert(0, (CharSequence) "Z"));
        p("buf cache insert(CharSequence,0,1)",
                () -> warm("abc").insert(0, (CharSequence) "Z", 0, 1));
        p("buf cache delete", () -> warm("abc").delete(0, 1));
        p("buf cache deleteCharAt", () -> warm("abc").deleteCharAt(0));
        p("buf cache replace", () -> warm("abc").replace(0, 1, "Z"));
        p("buf cache reverse", () -> warm("abc").reverse());
        p("buf cache setLength shorter", () -> { StringBuffer b = warm("abc"); b.setLength(1); return b; });
        p("buf cache setLength longer", () -> { StringBuffer b = warm("abc"); b.setLength(5); return b; });
        p("buf cache setCharAt", () -> { StringBuffer b = warm("abc"); b.setCharAt(0, 'Z'); return b; });
        p("buf cache repeat(char,int)", () -> warm("abc").repeat('Z', 2));
        p("buf cache repeat(cs,int)", () -> warm("abc").repeat("Z", 2));
        p("buf cache trimToSize keeps content",
                () -> { StringBuffer b = warm("abc"); b.trimToSize(); return b; });
        p("buf cache ensureCapacity keeps content",
                () -> { StringBuffer b = warm("abc"); b.ensureCapacity(99); return b; });
        // Two consecutive reads of an unmutated buffer must agree.
        p("buf cache stable without mutation", () -> {
            StringBuffer b = warm("abc");
            return b.toString().equals(b.toString());
        });
        // A failed mutator must not corrupt the cache either.
        p("buf cache after a throwing mutator", () -> {
            StringBuffer b = warm("abc");
            try {
                b.deleteCharAt(9);
            } catch (Throwable ignored) {
                // expected
            }
            return b.toString();
        });
        // `getChars` and `substring` read the cache-adjacent state.
        p("buf substring after mutation", () -> {
            StringBuffer b = warm("abcde");
            b.append("XY");
            return b.substring(4);
        });
        p("buf getChars after mutation", () -> {
            StringBuffer b = warm("abc");
            b.append("XY");
            char[] d = new char[5];
            b.getChars(0, 5, d, 0);
            return new String(d);
        });
        p("buf length after mutation", () -> {
            StringBuffer b = warm("abc");
            b.append("XY");
            return b.length();
        });
        p("buf charAt after mutation", () -> {
            StringBuffer b = warm("abc");
            b.append("XY");
            return b.charAt(4);
        });
    }

    // ------------------------------------------ L. the shared-surface contract
    //
    // `AbstractStringBuilder` is package-private and abstract, so anything
    // registered on it runs for BOTH subclasses and is reachable only through
    // them or through the interfaces they implement. These rows enter by the
    // interface doors rather than the concrete class.

    static void shared() {
        p("Appendable.append(CharSequence) on builder", () -> {
            Appendable a = new StringBuilder("a");
            a.append("bc");
            return a;
        });
        p("Appendable.append(null) on builder", () -> {
            Appendable a = new StringBuilder("a");
            a.append(null);
            return a;
        });
        p("Appendable.append(cs,1,3) on builder", () -> {
            Appendable a = new StringBuilder("a");
            a.append("wxyz", 1, 3);
            return a;
        });
        p("Appendable.append(null,1,3) on builder", () -> {
            Appendable a = new StringBuilder("a");
            a.append(null, 1, 3);
            return a;
        });
        p("Appendable.append(char) on builder", () -> {
            Appendable a = new StringBuilder("a");
            a.append('Z');
            return a;
        });
        p("Appendable.append(CharSequence) on buffer", () -> {
            Appendable a = new StringBuffer("a");
            a.append("bc");
            return a;
        });
        p("Appendable.append(null) on buffer", () -> {
            Appendable a = new StringBuffer("a");
            a.append(null);
            return a;
        });
        p("Appendable.append(null,1,3) on buffer", () -> {
            Appendable a = new StringBuffer("a");
            a.append(null, 1, 3);
            return a;
        });
        p("CharSequence.length on builder", () -> ((CharSequence) new StringBuilder("abc")).length());
        p("CharSequence.charAt on builder",
                () -> ((CharSequence) new StringBuilder("abc")).charAt(1));
        p("CharSequence.isEmpty on builder",
                () -> ((CharSequence) new StringBuilder("")).isEmpty());
        p("CharSequence.subSequence on buffer",
                () -> ((CharSequence) new StringBuffer("abc")).subSequence(1, 3));
        p("CharSequence.toString on buffer",
                () -> ((CharSequence) new StringBuffer("abc")).toString());
        p("CharSequence.chars on buffer",
                () -> Arrays.toString(((CharSequence) new StringBuffer(PAIR)).chars().toArray()));
        // The builders round-trip through String's own builder-aware doors.
        p("String.valueOf(builder)", () -> String.valueOf(new StringBuilder("abc")));
        p("new String(builder)", () -> new String(new StringBuilder("a" + PAIR)));
        p("new String(buffer)", () -> new String(new StringBuffer("a" + PAIR)));
        p("String.contentEquals(builder)", () -> "abc".contentEquals(new StringBuilder("abc")));
        p("String.contentEquals(buffer)", () -> "abc".contentEquals(new StringBuffer("abc")));
        p("String.contentEquals(builder) lone",
                () -> LONE_HI.contentEquals(new StringBuilder(LONE_HI)));
        p("String.join with builder", () -> String.join("-", new StringBuilder("a"), "b"));
        p("String.concat of builder toString", () -> "x".concat(new StringBuilder("y").toString()));
        p("builder compareTo equal",
                () -> new StringBuilder("abc").compareTo(new StringBuilder("abc")));
        p("builder compareTo less",
                () -> new StringBuilder("abc").compareTo(new StringBuilder("abd")));
        p("builder compareTo prefix",
                () -> new StringBuilder("ab").compareTo(new StringBuilder("abc")));
        p("buffer compareTo equal",
                () -> new StringBuffer("abc").compareTo(new StringBuffer("abc")));
        p("buffer compareTo greater",
                () -> new StringBuffer("abd").compareTo(new StringBuffer("abc")));
        p("builder equals is identity",
                () -> new StringBuilder("a").equals(new StringBuilder("a")));
        // A builder appended to itself: the JDK reads the source length ONCE.
        p("builder append itself", () -> {
            StringBuilder b = new StringBuilder("abc");
            b.append(b);
            return b;
        });
        p("buffer append itself", () -> {
            StringBuffer b = new StringBuffer("abc");
            b.append(b);
            return b;
        });
        p("builder insert itself", () -> {
            StringBuilder b = new StringBuilder("abc");
            b.insert(1, b);
            return b;
        });
        p("builder append buffer of itself", () -> {
            StringBuffer b = new StringBuffer("abc");
            StringBuilder s = new StringBuilder("Q");
            s.append(b);
            return s;
        });
        // A user subclass of CharSequence handed to the shared surface: the
        // registrar sits on the abstract base, so a shim that reads the
        // sequence by any route other than `charAt` answers differently.
        p("append(cs) whose toString lies", () -> new StringBuilder().append(new Seq("ab", "LIE")));
        p("insert(cs) whose toString lies",
                () -> new StringBuilder("Q").insert(0, new Seq("ab", "LIE")));
        p("append(cs,0,2) whose toString lies",
                () -> new StringBuilder().append(new Seq("ab", "LIE"), 0, 2));
        p("append(Object cs) uses toString",
                () -> new StringBuilder().append((Object) new Seq("ab", "LIE")));
    }

    // ------------------------------------------------ M. long / mixed content

    static void mixed(String pre, Mk mk) {
        p(pre + "chained append", () -> mk.of("a").append("b").toString() + "|");
        p(pre + "build 1 2 true", () -> {
            B b = mk.of("abc");
            b.append(1);
            b.append(true);
            b.append('x');
            b.append("y");
            return b.raw();
        });
        p(pre + "latin1 then utf16 then latin1", () -> {
            B b = mk.of("ab");
            b.append(EURO);
            b.append("cd");
            return b.raw();
        });
        p(pre + "utf16 then delete back to latin1", () -> {
            B b = mk.of("ab");
            b.append(EURO);
            b.delete(2, 3);
            return b.raw();
        });
        p(pre + "insert utf16 into latin1", () -> {
            B b = mk.of("abcd");
            b.insert(2, EURO);
            return b.raw();
        });
        p(pre + "reverse mixed", () -> {
            B b = mk.of("ab");
            b.append(EURO);
            return b.reverse();
        });
        p(pre + "long build length", () -> {
            B b = mk.of("");
            for (int i = 0; i < 300; i++) {
                b.append(i % 10);
            }
            return b.length();
        });
        p(pre + "long build tail", () -> {
            B b = mk.of("");
            for (int i = 0; i < 300; i++) {
                b.append(i % 10);
            }
            return b.raw().toString().substring(290);
        });
        p(pre + "long build then reverse head", () -> {
            B b = mk.of("");
            for (int i = 0; i < 300; i++) {
                b.append((char) ('a' + i % 26));
            }
            b.reverse();
            return b.raw().toString().substring(0, 10);
        });
        p(pre + "delete all then rebuild", () -> {
            B b = mk.of("abcdefghij");
            b.delete(0, 10);
            b.append("Z");
            return b.raw();
        });
        p(pre + "setLength truncate then toString", () -> {
            B b = mk.of("abcdefghij");
            b.setLength(4);
            return b.raw();
        });
        p(pre + "insert at every position", () -> {
            B b = mk.of("abc");
            b.insert(0, "0");
            b.insert(2, "1");
            b.insert(b.length(), "2");
            return b.raw();
        });
    }

    // ------------------------------- N. what real bytecode reads DIRECTLY
    //
    // `register_string_builder_natives` covers 62 (name, descriptor) pairs, and
    // the shared surface has more public API than that. Whatever it does not
    // cover runs as real `AbstractStringBuilder` bytecode, and that bytecode
    // reads the `value` / `coder` / `count` FIELDS rather than the `getValue()`
    // / `getCoder()` accessors — a field read no registered native can
    // intercept.
    //
    // The trap this section exists to avoid: every one of these rows PASSES on
    // pure-ASCII content, because a LATIN1 read of a `char[]` whose units are
    // all below 0x100 truncates to exactly the right bytes. The defect only
    // shows above 0x100, which is why each row below is asked twice — once on
    // ASCII, where a wrong answer would be a different bug, and once on
    // content that a LATIN1 misread cannot survive.

    static void direct(String pre, Mk mk) {
        p(pre + "chars ascii", () -> Arrays.toString(mk.of("abc").chars()));
        p(pre + "chars utf16", () -> Arrays.toString(mk.of("a" + EURO + "b").chars()));
        p(pre + "chars all utf16", () -> Arrays.toString(mk.of("" + EURO + EURO).chars()));
        p(pre + "codePoints ascii", () -> Arrays.toString(mk.of("abc").codePoints()));
        p(pre + "codePoints utf16", () -> Arrays.toString(mk.of("a" + EURO + "b").codePoints()));
        p(pre + "chars after an inflating append", () -> {
            B b = mk.of("ab");
            b.append(EURO);
            return Arrays.toString(b.chars());
        });
        p(pre + "chars count matches length", () -> {
            B b = mk.of("a" + EURO + "b");
            return b.chars().length == b.length();
        });
        p(pre + "chars of a builder grown past its first array", () -> {
            B b = mk.of("");
            for (int i = 0; i < 20; i++) {
                b.append(EURO);
            }
            return b.chars().length;
        });
    }

    static void compare() {
        p("sb compareTo ascii equal",
                () -> new StringBuilder("abc").compareTo(new StringBuilder("abc")));
        p("sb compareTo utf16 equal", () -> new StringBuilder("a" + EURO)
                .compareTo(new StringBuilder("a" + EURO)));
        p("sb compareTo utf16 differing", () -> new StringBuilder("a" + EURO)
                .compareTo(new StringBuilder("a\u20ad")));
        p("sb compareTo utf16 vs latin1", () -> sgn(new StringBuilder("a" + EURO)
                .compareTo(new StringBuilder("ab"))));
        p("sb compareTo latin1 vs utf16", () -> sgn(new StringBuilder("ab")
                .compareTo(new StringBuilder("a" + EURO))));
        p("sb compareTo high latin1 vs ascii",
                () -> sgn(new StringBuilder("\u00ff").compareTo(new StringBuilder("a"))));
        p("sb compareTo pair", () -> sgn(new StringBuilder(PAIR)
                .compareTo(new StringBuilder("z"))));
        p("sb compareTo self", () -> {
            StringBuilder b = new StringBuilder("a" + EURO);
            return b.compareTo(b);
        });
        p("sb compareTo prefix utf16", () -> sgn(new StringBuilder("a" + EURO)
                .compareTo(new StringBuilder("a" + EURO + "c"))));
        p("bf compareTo utf16 equal", () -> new StringBuffer("a" + EURO)
                .compareTo(new StringBuffer("a" + EURO)));
        p("bf compareTo utf16 differing", () -> new StringBuffer("a" + EURO)
                .compareTo(new StringBuffer("a\u20ad")));
        p("bf compareTo utf16 vs latin1", () -> sgn(new StringBuffer("a" + EURO)
                .compareTo(new StringBuffer("ab"))));
        p("bf compareTo after a mutation", () -> {
            StringBuffer x = new StringBuffer("a");
            x.append(EURO);
            return x.compareTo(new StringBuffer("a" + EURO));
        });
    }

    /** `compareTo`'s magnitude is a code-unit difference, which is a legitimate
     *  implementation choice; only its SIGN is contract. */
    static int sgn(int v) {
        return v < 0 ? -1 : (v > 0 ? 1 : 0);
    }

    static void serial(String pre, boolean buffer) {
        p(pre + "serialize round trip ascii", () -> roundTrip(buffer, "abc"));
        p(pre + "serialize round trip utf16", () -> roundTrip(buffer, "a" + EURO + "b"));
        p(pre + "serialize round trip pair", () -> roundTrip(buffer, "a" + PAIR));
        p(pre + "serialize round trip empty", () -> "[" + roundTrip(buffer, "") + "]");
        p(pre + "serialize round trip length", () -> {
            Object o = roundTripObject(buffer, "a" + EURO + "b");
            return ((CharSequence) o).length();
        });
    }

    static String roundTrip(boolean buffer, String s) throws Exception {
        return String.valueOf(roundTripObject(buffer, s));
    }

    static Object roundTripObject(boolean buffer, String s) throws Exception {
        Object src = buffer ? new StringBuffer(s) : new StringBuilder(s);
        java.io.ByteArrayOutputStream bo = new java.io.ByteArrayOutputStream();
        java.io.ObjectOutputStream oo = new java.io.ObjectOutputStream(bo);
        oo.writeObject(src);
        oo.flush();
        java.io.ObjectInputStream oi =
                new java.io.ObjectInputStream(new java.io.ByteArrayInputStream(bo.toByteArray()));
        return oi.readObject();
    }

    // ------------------------------------ O. the two contracts that differ
    //
    // `StringBuffer` is synchronized and `StringBuilder` is not, and every
    // mutator the registrar covers replaces a synchronized JDK body. A native
    // that does not take the receiver's monitor loses updates under contention
    // and never throws.
    //
    // ONE-WAY: a short final length is a real defect; the exact number it
    // reaches is not, so only the "did every update land" predicate is
    // printed, never a count. The full length is the only deterministic answer
    // — a correct implementation reaches it whatever the host load does.

    static void sync() {
        p("buffer append is mutually exclusive", () -> contend(true, 4000));
        p("buffer insert is mutually exclusive", () -> contendInsert(4000));
        p("buffer declares synchronized append", () -> java.lang.reflect.Modifier
                .isSynchronized(StringBuffer.class.getMethod("append", String.class)
                        .getModifiers()));
        p("builder does not declare synchronized append", () -> java.lang.reflect.Modifier
                .isSynchronized(StringBuilder.class.getMethod("append", String.class)
                        .getModifiers()));
        p("buffer declares synchronized toString", () -> java.lang.reflect.Modifier
                .isSynchronized(StringBuffer.class.getMethod("toString").getModifiers()));
        p("buffer declares synchronized length", () -> java.lang.reflect.Modifier
                .isSynchronized(StringBuffer.class.getMethod("length").getModifiers()));
    }

    static Object contend(boolean buffer, int each) throws Exception {
        final StringBuffer b = new StringBuffer();
        Runnable r = () -> {
            for (int i = 0; i < each; i++) {
                b.append('x');
            }
        };
        Thread t1 = new Thread(r);
        Thread t2 = new Thread(r);
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        return b.length() == each * 2 && b.toString().length() == each * 2;
    }

    static Object contendInsert(int each) throws Exception {
        final StringBuffer b = new StringBuffer();
        Runnable r = () -> {
            for (int i = 0; i < each; i++) {
                b.insert(0, 'y');
            }
        };
        Thread t1 = new Thread(r);
        Thread t2 = new Thread(r);
        t1.start();
        t2.start();
        t1.join();
        t2.join();
        return b.length() == each * 2;
    }

    // ------------------------------- P. the remaining null and ordering edges

    static void nulls(String pre, Mk mk) {
        p(pre + "lastIndexOf(null,0)", () -> mk.of("abc").lastIndexOf(null, 0));
        p(pre + "insert(9,(char[])null) offset checked first",
                () -> mk.of("abc").insert(9, (char[]) null));
        p(pre + "insert(-1,(char[])null,0,1)",
                () -> mk.of("abc").insert(-1, (char[]) null, 0, 1));
        p(pre + "append((char[])null,1,2)", () -> mk.of("abc").appendChars(null, 1, 2));
        p(pre + "append((char[])null,-1,-1)", () -> mk.of("abc").appendChars(null, -1, -1));
        p(pre + "indexOf(null,-1)", () -> mk.of("abc").indexOf(null, -1));
        p(pre + "indexOf(null,99)", () -> mk.of("abc").indexOf(null, 99));
        p(pre + "lastIndexOf(null,99)", () -> mk.of("abc").lastIndexOf(null, 99));
        p(pre + "indexOf(null) on empty", () -> mk.of("").indexOf(null));
        p(pre + "replace(1,2,(String)null) after a valid range",
                () -> mk.of("abcde").replace(1, 2, null));
        p(pre + "insert(0,(String)null) at a valid offset",
                () -> mk.of("abc").insert(0, (String) null));
        p(pre + "insert(-1,(String)null) offset first",
                () -> mk.of("abc").insert(-1, (String) null));
        p(pre + "insert(-1,(CharSequence)null) offset first",
                () -> mk.of("abc").insertCs(-1, null));
        p(pre + "append(null CharSequence,3,1) reversed window",
                () -> mk.of("a").appendCs(null, 3, 1));
        p(pre + "append(null CharSequence,-1,2)", () -> mk.of("a").appendCs(null, -1, 2));
    }

    // ------------------------------- Q. non-LATIN1 content through every door

    static void wide(String pre, Mk mk) {
        p(pre + "new(utf16) capacity", () -> mk.of("a" + EURO).capacity());
        p(pre + "new(utf16) toString", () -> mk.of("a" + EURO).raw());
        p(pre + "charAt on utf16", () -> (int) mk.of("a" + EURO).charAt(1));
        p(pre + "substring on utf16", () -> mk.of("a" + EURO + "b").substring(1, 2));
        p(pre + "indexOf on utf16", () -> mk.of("a" + EURO + "b").indexOf(String.valueOf(EURO)));
        p(pre + "lastIndexOf on utf16",
                () -> mk.of("a" + EURO + "b" + EURO).lastIndexOf(String.valueOf(EURO)));
        p(pre + "getChars on utf16", () -> {
            char[] d = new char[3];
            mk.of("a" + EURO + "b").getChars(0, 3, d, 0);
            return (int) d[1];
        });
        p(pre + "delete back to all-latin1 keeps capacity", () -> {
            B b = mk.of("ab");
            b.append(EURO);
            int before = b.capacity();
            b.deleteCharAt(2);
            return b.capacity() == before;
        });
        p(pre + "trimToSize on utf16 keeps content", () -> {
            B b = mk.of("a" + EURO);
            b.trimToSize();
            return b.raw();
        });
        p(pre + "trimToSize on utf16 capacity", () -> {
            B b = mk.of("a" + EURO);
            b.trimToSize();
            return b.capacity();
        });
        p(pre + "setLength grows utf16 with NULs", () -> {
            B b = mk.of("a" + EURO);
            b.setLength(4);
            return b.raw();
        });
        p(pre + "reverse utf16", () -> mk.of("a" + EURO + "b").reverse());
        p(pre + "replace inside utf16", () -> mk.of("a" + EURO + "b").replace(1, 2, "Q"));
        p(pre + "subSequence of utf16", () -> mk.of("a" + EURO + "b").subSequence(0, 2));
        p(pre + "String.valueOf of utf16 builder",
                () -> String.valueOf(mk.of("a" + EURO + "b").raw()));
        p(pre + "contentEquals of utf16 builder",
                () -> ("a" + EURO + "b").contentEquals((CharSequence) mk.of("a" + EURO + "b").raw()));
        p(pre + "concat of utf16 builder", () -> "x" + mk.of("a" + EURO).raw());
        p(pre + "format of utf16 builder",
                () -> String.format("%s", mk.of("a" + EURO).raw()));
        p(pre + "codePointCount over utf16", () -> mk.of("a" + EURO + "b").codePointCount(0, 3));
        p(pre + "isEmpty on utf16", () -> mk.of("" + EURO).isEmpty());
        p(pre + "appending into a Writer", () -> {
            java.io.StringWriter w = new java.io.StringWriter();
            w.append(mk.of("a" + EURO).raw().toString());
            return w.toString();
        });
    }

    // ------------------------------------------------------------------ main

    public static void main(String[] args) {
        sect("builder-construction", () -> construction("sb ", false));
        sect("buffer-construction", () -> construction("bf ", true));
        sect("builder-append", () -> appends("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-append", () -> appends("bf ", StringBuilderShadowSweep::bf));
        sect("builder-insert", () -> inserts("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-insert", () -> inserts("bf ", StringBuilderShadowSweep::bf));
        sect("builder-mutators", () -> mutators("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-mutators", () -> mutators("bf ", StringBuilderShadowSweep::bf));
        sect("builder-readers", () -> readers("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-readers", () -> readers("bf ", StringBuilderShadowSweep::bf));
        sect("builder-getChars", () -> getChars("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-getChars", () -> getChars("bf ", StringBuilderShadowSweep::bf));
        sect("builder-reverse", () -> reverse("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-reverse", () -> reverse("bf ", StringBuilderShadowSweep::bf));
        sect("builder-search", () -> search("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-search", () -> search("bf ", StringBuilderShadowSweep::bf));
        sect("builder-capacity", () -> capacity("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-capacity", () -> capacity("bf ", StringBuilderShadowSweep::bf));
        sect("builder-repeat", () -> repeat("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-repeat", () -> repeat("bf ", StringBuilderShadowSweep::bf));
        sect("buffer-cache", StringBuilderShadowSweep::cache);
        sect("shared-surface", StringBuilderShadowSweep::shared);
        sect("builder-mixed", () -> mixed("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-mixed", () -> mixed("bf ", StringBuilderShadowSweep::bf));
        sect("builder-direct", () -> direct("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-direct", () -> direct("bf ", StringBuilderShadowSweep::bf));
        sect("compare", StringBuilderShadowSweep::compare);
        sect("builder-serial", () -> serial("sb ", false));
        sect("buffer-serial", () -> serial("bf ", true));
        sect("sync", StringBuilderShadowSweep::sync);
        sect("builder-nulls", () -> nulls("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-nulls", () -> nulls("bf ", StringBuilderShadowSweep::bf));
        sect("builder-wide", () -> wide("sb ", StringBuilderShadowSweep::sb));
        sect("buffer-wide", () -> wide("bf ", StringBuilderShadowSweep::bf));
        System.out.println("rows " + rows);
        System.out.println("DONE StringBuilderShadowSweep");
    }
}
