// Interpreter round i1 wave 20, lane L5: the two-slot negative `instanceof`
// memo (`CastSite::negative_receivers`,
// docs/internal/fixed-bugs/interpreter-L4-proposal-polymorphic-negative-cast-memo-FIXED-20261003.md).
//
// One `instanceof` ladder whose receiver rotates through six objects: plain
// classes (whose refusals the memo keeps, two per rung, evicting the oldest),
// a lambda and a dynamic proxy (instance-dependent, never memoised). The
// first rung refuses five receiver classes in rotation, so its two slots are
// rewritten constantly; a wrong slot answer would move a count. Run under
// --compatible with and without --nojit; HotSpot 25 prints:
//   pass0 counts=10000,20000,10000,10000,10000
//   pass1 counts=10000,20000,10000,10000,10000
//   pass2 counts=10000,20000,10000,10000,10000
public class L5W20NegativeLadder {
    interface Shape {}
    interface Named {}
    static class A implements Shape {}
    static class B {}
    static class C extends B implements Named {}
    static class D {}

    static String ladder(Object[] xs, int rotate) {
        int[] counts = new int[5];
        int n = xs.length;
        for (int i = 0; i < 10000 * n; i++) {
            Object o = xs[(i + rotate) % n];
            if (o instanceof Shape) counts[0]++;
            else if (o instanceof Named) counts[1]++;
            else if (o instanceof B) counts[2]++;
            else if (o instanceof java.util.function.Supplier) counts[3]++;
            else counts[4]++;
        }
        StringBuilder sb = new StringBuilder();
        for (int k = 0; k < counts.length; k++) {
            if (k > 0) sb.append(',');
            sb.append(counts[k]);
        }
        return sb.toString();
    }

    public static void main(String[] args) {
        java.util.function.Supplier<String> lambda = () -> "x";
        Object proxy = java.lang.reflect.Proxy.newProxyInstance(
                L5W20NegativeLadder.class.getClassLoader(),
                new Class<?>[] { Named.class },
                (p, m, a) -> null);
        Object[] xs = { new A(), new B(), new C(), new D(), lambda, proxy };
        System.out.println("pass0 counts=" + ladder(xs, 0));
        // Another order: the slots see the refusals in another sequence.
        Object[] ys = { proxy, new D(), lambda, new C(), new B(), new A() };
        System.out.println("pass1 counts=" + ladder(ys, 0));
        System.out.println("pass2 counts=" + ladder(xs, 3));
    }
}
