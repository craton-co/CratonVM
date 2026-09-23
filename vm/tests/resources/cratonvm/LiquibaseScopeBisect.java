package cratonvm;

// Mirrors the original BUG-LQB-SCOPE repro shape as closely as possible:
// a static counter mutation (putstatic-equivalent: an instance field write,
// since this fixture avoids clinit complications) immediately followed by a
// string-concat invokedynamic, invoked many times from a JIT-compiled
// caller. If the (now-removed) BUG-LQB-SCOPE gate's protection is still
// needed, this would show the counter incremented ~2x the actual call
// count once the callee JIT-compiles.
public class LiquibaseScopeBisect {
    static int counter = 0;

    // enter()-shaped: putstatic-equivalent side effect, then indy concat.
    static String enter(int x) {
        counter++;
        return "scope-" + x;
    }

    public static int checksum() {
        int sum = 0;
        for (int i = 0; i < 3_000_000; i++) {
            sum += enter(i).length();
        }
        return sum;
    }

    public static int counterValue() {
        return counter;
    }

    // Nested variant: a JIT-compiled caller directly calls a JIT-compiled
    // callee containing the trap (probes the nested-identity-mismatch
    // fallback specifically).
    static int nestedCaller(int x) {
        return enter(x).length();
    }

    public static int nestedChecksum() {
        counter = 0;
        int sum = 0;
        for (int i = 0; i < 3_000_000; i++) {
            sum += nestedCaller(i);
        }
        return sum;
    }

    public static void main(String[] args) {
        counter = 0;
        int c1 = checksum();
        int counterAfter1 = counterValue();
        System.out.println("checksum=" + c1 + " counterAfter=" + counterAfter1);

        int c2 = nestedChecksum();
        int counterAfter2 = counterValue();
        System.out.println("nestedChecksum=" + c2 + " counterAfter=" + counterAfter2);
    }
}
