import java.util.Objects;

/**
 * A static method registered as a native is never JIT-compiled and never
 * inlined: `dispatch_static` does not consult
 * `force_native_over_real_jdk_bytecode`, so the registry always wins for a
 * static, even when the real JDK bytecode is present and trivial.
 *
 * Each rung calls one such method N times, against a byte-identical copy
 * declared in THIS class (which is an ordinary Java static and does compile).
 */
public class StaticNativeBench {

    static int sink;
    static Object osink;

    // Byte-identical local twins of the JDK statics.
    static boolean myEquals(Object a, Object b) { return (a == b) || (a != null && a.equals(b)); }
    static int myHashCode(Object o) { return o != null ? o.hashCode() : 0; }
    static <T> T myRequireNonNull(T o) { if (o == null) { throw new NullPointerException(); } return o; }
    static boolean myIsNull(Object o) { return o == null; }

    public static void main(String[] args) {
        String rung = args.length > 0 ? args[0] : "equals";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 20_000_000;

        String a = "test-7-property-42";
        Object o = a;

        long t0 = System.nanoTime();
        for (int i = 0; i < n; i++) {
            switch (rung) {
                case "equals"     -> { if (Objects.equals(a, a)) { sink++; } }
                case "myEquals"   -> { if (myEquals(a, a)) { sink++; } }
                case "hashCode"   -> { sink += Objects.hashCode(o); }
                case "myHashCode" -> { sink += myHashCode(o); }
                case "reqNonNull" -> { osink = Objects.requireNonNull(o); }
                case "myReqNN"    -> { osink = myRequireNonNull(o); }
                case "isNull"     -> { if (Objects.isNull(o)) { sink++; } }
                case "myIsNull"   -> { if (myIsNull(o)) { sink++; } }
                default -> throw new IllegalArgumentException(rung);
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.printf("STATICNAT rung=%-11s n=%d ms=%d ns/call=%.1f sink=%d%n",
                rung, n, ms, (ms * 1e6) / n, sink);
    }
}
