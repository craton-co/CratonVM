import java.text.DateFormatSymbols;
import java.util.Locale;

/**
 * Isolates `java.text.DateFormatSymbols.getProviderInstance(Locale)` — the
 * hot JDK method known-issue
 * `dateformatsymbols-getproviderinstance-compile-bail-20260731` reports as
 * permanently failing codegen (`tier_fail_count=3`, `backend_attempted=true`,
 * no named reason).
 *
 * `DateFormatSymbols.getInstance(Locale)` is a two-instruction wrapper whose
 * only real work is `invokestatic getProviderInstance`, so a loop over it
 * drives the target and nothing else. The result is only null-tested, not
 * read: `getMonths()` clones a 13-element array per iteration and would bury
 * the method being measured.
 */
public final class DfsProviderProbe {

    private static final Locale US = Locale.US;

    public static void main(String[] args) {
        int iters = Integer.getInteger("probe.iters", 2000);
        // Warm-up: the JIT's invocation counter has to cross its threshold
        // before a compile is even attempted, and a compile-bail is only
        // interesting once it has been attempted (and retried) three times.
        run(iters);
        long t0 = System.nanoTime();
        int sink = run(iters);
        long t1 = System.nanoTime();
        System.out.println(
            "DateFormatSymbols.getInstance  ns/op=" + ((t1 - t0) / iters) + "  sink=" + sink);
    }

    private static int run(int iters) {
        int sink = 0;
        for (int i = 0; i < iters; i++) {
            DateFormatSymbols dfs = DateFormatSymbols.getInstance(US);
            if (dfs != null) {
                sink++;
            }
        }
        return sink;
    }
}
