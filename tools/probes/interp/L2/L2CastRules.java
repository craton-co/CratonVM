// Interpreter round i1, wave 10, lane L2 — checkcast refuses what HotSpot
// refuses, in every mode and both tiers.
//
// Wave 10 deleted `checkcast`'s three caller-name hatches (one turned a
// failed cast of a bare `Object` to a ClassLoader type into the app class
// loader under --compatible) and retired the lenient `Object[] -> T[]` array
// cast in every mode. Compare stdout against HotSpot 25 (`java L2CastRules`);
// run CratonVM with `--compatible`, with and without `--nojit` (the warm rows
// drive the compiled `checkcast` helper).
//
// Expected HotSpot 25 stdout:
//   c1 (ClassLoader) Object: ClassCastException
//   c2 (SecureClassLoader) Object: ClassCastException
//   c3 (String[][]) Object[][]: ClassCastException
//   c4 (Object[]) byte[]: ClassCastException
//   c5 (Integer[]) Object[]: ClassCastException
//   c6 (Object[]) String[]: ok
//   w1 warm (String[]) Object[]: cce=200000
//   w2 warm (Object[]) int[]: cce=200000
//   w3 warm (Number[]) Integer[]: ok=200000
public class L2CastRules {
    static String row(Runnable r) {
        try {
            r.run();
            return "ok";
        } catch (ClassCastException e) {
            return "ClassCastException";
        }
    }

    static Object opaque(Object o) {
        return o;
    }

    static int warmStrings(Object o, int n) {
        int cce = 0;
        for (int i = 0; i < n; i++) {
            try {
                String[] s = (String[]) o;
                if (s == null) cce--;
            } catch (ClassCastException e) {
                cce++;
            }
        }
        return cce;
    }

    static int warmObjects(Object o, int n) {
        int cce = 0;
        for (int i = 0; i < n; i++) {
            try {
                Object[] a = (Object[]) o;
                if (a == null) cce--;
            } catch (ClassCastException e) {
                cce++;
            }
        }
        return cce;
    }

    static int warmNumbers(Object o, int n) {
        int ok = 0;
        for (int i = 0; i < n; i++) {
            Number[] a = (Number[]) o;
            if (a.length == 2) ok++;
        }
        return ok;
    }

    public static void main(String[] args) {
        System.out.println("c1 (ClassLoader) Object: "
                + row(() -> { ClassLoader cl = (ClassLoader) opaque(new Object()); }));
        System.out.println("c2 (SecureClassLoader) Object: "
                + row(() -> {
                    java.security.SecureClassLoader cl =
                            (java.security.SecureClassLoader) opaque(new Object());
                }));
        System.out.println("c3 (String[][]) Object[][]: "
                + row(() -> { String[][] s = (String[][]) opaque(new Object[1][]); }));
        System.out.println("c4 (Object[]) byte[]: "
                + row(() -> { Object[] a = (Object[]) opaque(new byte[1]); }));
        System.out.println("c5 (Integer[]) Object[]: "
                + row(() -> { Integer[] a = (Integer[]) opaque(new Object[] {1, 2}); }));
        System.out.println("c6 (Object[]) String[]: "
                + row(() -> { Object[] a = (Object[]) opaque(new String[] {"x"}); }));
        System.out.println("w1 warm (String[]) Object[]: cce="
                + warmStrings(opaque(new Object[1]), 200000));
        System.out.println("w2 warm (Object[]) int[]: cce="
                + warmObjects(opaque(new int[1]), 200000));
        System.out.println("w3 warm (Number[]) Integer[]: ok="
                + warmNumbers(opaque(new Integer[] {1, 2}), 200000));
    }
}
