import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;

/**
 * nio21 wave 6 L1: witness for
 * docs/internal/fixed-bugs/r21-stringlatin1-kc26-intrinsics-are-funnel-calls-with-non-hotspot-failures-FIXED-20261013.md.
 *
 * Calls the KC26 `java.lang.StringLatin1` methods (`getChar`, both `compareTo`s, `inflate`)
 * through reflection with valid and with bad arguments and prints each answer: the value, or
 * the exception's class, message and top two stack frames (2026-10-13: a faulting `compareTo`
 * re-enters the JDK body, so the `ArraysSupport.mismatch` frame is HotSpot's; `getChar` is plain
 * bytecode).
 *
 * Output (conformance, deterministic): one `latin1 <row> <answer>` line per case.
 *
 * Run:
 *   javac -d out tools/probes/Nio21Latin1Faults.java
 *   java --add-opens java.base/java.lang=ALL-UNNAMED -cp out Nio21Latin1Faults > hs.txt
 *   cratonvm --jdk-only   --add-opens java.base/java.lang=ALL-UNNAMED -cp out Nio21Latin1Faults > jo.txt
 *   cratonvm --compatible --add-opens java.base/java.lang=ALL-UNNAMED -cp out Nio21Latin1Faults > co.txt
 * All three identical (also with --nojit). CRATONVM_LATIN1_HOTSPOT_FAULTS=0 restores the old
 * `compareTo` answers (rows differ).
 */
public class Nio21Latin1Faults {
    static Method m(Class<?> c, String n, Class<?>... p) throws Exception {
        Method x = c.getDeclaredMethod(n, p);
        x.setAccessible(true);
        return x;
    }

    static void row(String name, Method m, Object... args) {
        String answer;
        try {
            Object r = m.invoke(null, args);
            // A char as its code, so the line does not depend on the console encoding.
            answer = r instanceof Character ch ? "char:" + (int) ch : String.valueOf(r);
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            StackTraceElement[] st = t.getStackTrace();
            answer = t.getClass().getName() + ": " + t.getMessage()
                    + " @ " + (st.length > 0 ? st[0] : "-") + " | " + (st.length > 1 ? st[1] : "-");
        } catch (Throwable t) {
            answer = "OUTER " + t.getClass().getName();
        }
        System.out.println("latin1 " + name + " " + answer);
    }

    public static void main(String[] a) throws Exception {
        Class<?> c = Class.forName("java.lang.StringLatin1");
        Method gc = m(c, "getChar", byte[].class, int.class);
        Method c2 = m(c, "compareTo", byte[].class, byte[].class);
        Method c4 = m(c, "compareTo", byte[].class, byte[].class, int.class, int.class);
        Method inf = m(c, "inflate", byte[].class, int.class, char[].class, int.class, int.class);
        byte[] hi = {(byte) 0xff, 'a'};
        row("getchar-ok", gc, hi, 0);
        row("getchar-null", gc, null, 0);
        row("getchar-past", gc, new byte[3], 5);
        row("getchar-neg", gc, new byte[3], -1);
        row("cmp2-ok", c2, "abc".getBytes(), "abd".getBytes());
        row("cmp2-len", c2, "ab".getBytes(), "abc".getBytes());
        row("cmp2-unsigned", c2, hi, new byte[] {1, 'a'});
        row("cmp2-null-a", c2, null, new byte[1]);
        row("cmp2-null-b", c2, new byte[1], null);
        row("cmp4-ok", c4, "abcx".getBytes(), "abcy".getBytes(), 3, 3);
        row("cmp4-null-a", c4, null, new byte[1], 1, 1);
        row("cmp4-null-b", c4, new byte[2], null, 1, 1);
        row("cmp4-null-lim0", c4, null, null, 0, 3);
        row("cmp4-neg", c4, null, null, -4, 2);
        row("cmp4-past", c4, new byte[2], new byte[2], 3, 3);
        row("cmp4-diff-before-past", c4, new byte[] {1}, new byte[] {2}, 4, 4);
        row("cmp4-null-long", c4, null, new byte[9], 9, 9);
        row("inflate-null-src", inf, null, 0, new char[2], 0, 1);
        row("inflate-null-dst", inf, new byte[2], 0, null, 0, 1);
        row("inflate-null-len0", inf, null, 0, null, 0, 0);
    }
}
