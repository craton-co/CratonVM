import java.nio.charset.StandardCharsets;

/**
 * The `java/lang/String` shapes whose registrations carried a stated
 * dependency claim, and which the real-JDK `Bridge` drop removed.
 *
 * `probes/StringPolicyMatrixProbe` covers the methods the forced-native policy
 * named. It does **not** cover several constructors and helpers that were
 * registered for reasons of their own, each with a comment saying what breaks
 * without it. Those comments are claims about the real bytecode's
 * dependencies, and a class-wide drop invalidates every one of them at once —
 * so each needs its own case, run against HotSpot.
 *
 * The claims being tested, verbatim from the registration sites:
 *
 *   * `String(byte[])` / `String(byte[],int,int)` — "the real-JDK bytecode
 *     walks `Charset.defaultCharset()` / `sun.nio.cs.UTF_8.INSTANCE` and
 *     `StringCoding.countPositives` which require a working `sun.nio.cs`
 *     provider chain we don't fully bootstrap. Without these intercepts the
 *     constructors silently complete with zero `value` bytes and surface as
 *     EMPTY STRINGS."
 *   * `String(StringBuilder)` (DF05) — the real ctor reads the builder's
 *     `byte[]`, "CratonVM's synthetic StringBuilder backs its content with a
 *     char[], so that copy hits a char[]->byte[] mismatch
 *     (ArrayStoreException ...)".
 *   * `String(byte[],int,int,int)` — the deprecated hibyte ctor.
 *   * `String.indexOf(String,int)` and its package-private static helper
 *     (T19.H1) — "the helper returns 0 every iteration", corrupting cglib's
 *     `TypeUtils.map` loop.
 *
 * Empty output where content is expected is the signature of the first claim,
 * so every case prints the LENGTH alongside the value: a silent empty string
 * and a correct one differ by exactly that.
 */
public class StringDroppedNativesProbe {

    static int n = 0;

    static void emit(String shape, Object v) {
        n++;
        String rendered;
        if (v == null) {
            rendered = "null";
        } else if (v instanceof String) {
            String s = (String) v;
            rendered = "len=" + s.length() + " \"" + escape(s) + "\"";
        } else {
            rendered = String.valueOf(v);
        }
        System.out.println(n + " " + shape + " => " + rendered);
    }

    static void expectThrow(String shape, Thrower body) {
        n++;
        try {
            Object v = body.run();
            String rendered = (v instanceof String)
                    ? "len=" + ((String) v).length() + " \"" + escape((String) v) + "\""
                    : String.valueOf(v);
            System.out.println(n + " " + shape + " => NO-THROW " + rendered);
        } catch (Throwable t) {
            System.out.println(n + " " + shape + " => throws " + t.getClass().getName());
        }
    }

    interface Thrower {
        Object run() throws Throwable;
    }

    static String escape(String s) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c >= 0x20 && c < 0x7f && c != '"' && c != '\\') {
                sb.append(c);
            } else {
                sb.append(String.format("\\u%04X", (int) c));
            }
        }
        return sb.toString();
    }

    public static void main(String[] args) throws Exception {
        byte[] ascii = "Hello, World".getBytes(StandardCharsets.UTF_8);
        byte[] utf8 = "café ΣΟΣ".getBytes(StandardCharsets.UTF_8);
        byte[] high = new byte[] { (byte) 0xC3, (byte) 0xA9, 'a' };

        // ---- String(byte[]) family: the "silently empty" claim ------------
        emit("new String(ascii)", new String(ascii));
        emit("new String(utf8)", new String(utf8));
        emit("new String(high)", new String(high));
        emit("new String(new byte[0])", new String(new byte[0]));
        emit("new String(ascii,0,5)", new String(ascii, 0, 5));
        emit("new String(ascii,7,5)", new String(ascii, 7, 5));
        emit("new String(utf8,0,4)", new String(utf8, 0, 4));
        emit("roundtrip(byte[])", new String(utf8).equals("café ΣΟΣ"));
        expectThrow("new String(ascii,-1,2)", () -> new String(ascii, -1, 2));
        expectThrow("new String(ascii,0,999)", () -> new String(ascii, 0, 999));
        expectThrow("new String((byte[])null)", () -> new String((byte[]) null));

        // ---- the deprecated hibyte ctor -----------------------------------
        emit("new String(ascii,0,0,ascii.length)", new String(ascii, 0, 0, ascii.length));
        emit("new String(high,1,0,3)", new String(high, 1, 0, 3));

        // ---- String(StringBuilder): the DF05 ArrayStoreException claim ----
        StringBuilder sb = new StringBuilder();
        sb.append("abc").append('d').append(42).append('Σ');
        emit("new String(StringBuilder)", new String(sb));
        emit("sb.toString()", sb.toString());
        emit("new String(sb).equals(sb.toString())", new String(sb).equals(sb.toString()));
        StringBuilder empty = new StringBuilder();
        emit("new String(emptyBuilder)", new String(empty));
        StringBuffer sbuf = new StringBuffer("xyzİ");
        emit("new String(StringBuffer)", new String(sbuf));

        // ---- indexOf(String,int) and the static helper: T19.H1 ------------
        String hay = "abcabcabc";
        emit("indexOf(\"abc\",0)", hay.indexOf("abc", 0));
        emit("indexOf(\"abc\",1)", hay.indexOf("abc", 1));
        emit("indexOf(\"abc\",4)", hay.indexOf("abc", 4));
        emit("indexOf(\"abc\",7)", hay.indexOf("abc", 7));
        emit("indexOf(\"zz\",0)", hay.indexOf("zz", 0));
        emit("indexOf(\"\",3)", hay.indexOf("", 3));
        String utf16Hay = "ΣabΣab";
        emit("indexOf-utf16(\"ab\",1)", utf16Hay.indexOf("ab", 1));
        emit("indexOf-utf16(\"Σ\",1)", utf16Hay.indexOf("Σ", 1));
        // The cglib TypeUtils.map shape: walk every occurrence. A helper that
        // "returns 0 every iteration" makes this loop forever, so it is
        // bounded — a bound that trips is itself the failure signal.
        int count = 0, at = 0, guard = 0;
        while ((at = hay.indexOf("abc", at)) >= 0 && guard++ < 100) {
            count++;
            at += 1;
        }
        emit("scan-all-occurrences", "count=" + count + " guard=" + guard);

        // ---- getBytes round trips -----------------------------------------
        emit("getBytes().length", "café".getBytes().length);
        emit("getBytes(UTF-8) roundtrip",
                new String("café Σ".getBytes(StandardCharsets.UTF_8),
                        StandardCharsets.UTF_8).equals("café Σ"));

        System.out.println("DROPPED-NATIVES cases=" + n);
    }
}
