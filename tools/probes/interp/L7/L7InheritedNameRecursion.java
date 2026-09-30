// Interpreter round i1 wave 12, lane L3.
// Page: docs/internal/fixed-bugs/interpreter-L3-cycle-checks-miss-calls-through-an-inheriting-class-name-FIXED-20260925.md
//
// Recursion whose call sites name an INHERITING class: `Base.depth` calls
// `Sub.depth` (javac emits `invokestatic L7InheritedNameRecursion$Sub.depth`,
// which JVMS 5.4.3.3 resolves to `Base.depth`), and `Base.even`/`Base.odd`
// recurse into each other through `Sub`. The JIT's compile-time cycle checks
// compare the site's class name (`Sub`) with the compiling method's declaring
// class (`Base`), so they do not see these as self/mutual recursion.
//
// HotSpot 25 prints (identical with and without --nojit on CratonVM):
//   depth sum: 4900000
//   parity: 100000 even, 100000 odd
//   deep: StackOverflowError
//   done
//
// What to measure (stderr only): wall time is printed to stderr. Run with
// `CRATONVM_DBG=jit-method-stats` and compare `c1=`/`c2=` in the
// `JIT method stats:` line, or `CRATONVM_DBG_JITC=1` and count the
// `callee-compile ... L7InheritedNameRecursion$Base.depth` lines: one compile
// of `Base.depth` per tier is expected once the checks ask by declaring class.
public class L7InheritedNameRecursion {
    static class Base {
        static int depth(int n) {
            return n == 0 ? 0 : 1 + Sub.depth(n - 1);
        }

        static boolean even(int n) {
            return n == 0 || Sub.odd(n - 1);
        }

        static boolean odd(int n) {
            return n != 0 && Sub.even(n - 1);
        }
    }

    static class Sub extends Base {
    }

    public static void main(String[] args) {
        long start = System.nanoTime();
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            sum += Base.depth(i % 50);
        }
        System.out.println("depth sum: " + sum);

        int evens = 0;
        int odds = 0;
        for (int i = 0; i < 200_000; i++) {
            if (Sub.even(i % 40)) {
                evens++;
            } else {
                odds++;
            }
        }
        System.out.println("parity: " + evens + " even, " + odds + " odd");

        try {
            System.out.println("deep: " + Base.depth(50_000_000));
        } catch (StackOverflowError e) {
            System.out.println("deep: StackOverflowError");
        }
        System.err.println("L7InheritedNameRecursion ms=" + (System.nanoTime() - start) / 1_000_000);
        System.out.println("done");
    }
}
