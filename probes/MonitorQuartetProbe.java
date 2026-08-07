import java.util.*;

/**
 * Regression probe for the `monitorexit` header-quartet wipe.
 *
 * Since the header shrink (`HEADER_SIZE 24 -> 16`) an object's kind,
 * element_type, gc_flags and gc_age live in mark-word bits 48..63.
 * `try_thin_unlock`'s final-release arm stored a bare `MARK_NEUTRAL`, so the
 * first `synchronized` block on an object erased all four. Losing
 * `GC_FLAG_COMPACT` makes every reader that honours the per-object header
 * (i.e. every native `get_field`) decode the object's packed 8-byte reference
 * fields as 16-byte legacy `Value` cells, and the heap guard rejects the
 * result — so the field reads back as null.
 *
 * `Collections.synchronizedSet` is the cheapest victim: its two fields are
 * `c` (the backing collection) and `mutex` (== this), and `iterator()` reads
 * `c` through a native.
 *
 *   cratonvm --java-home $JDK25 --nojit -c . MonitorQuartetProbe
 *
 * Broken build: `PROBE-FAILURES=<n>`. Fixed build: `PROBE-OK`.
 * Deterministic; needs no JUnit, no H2, and no JIT.
 */
public class MonitorQuartetProbe {

    static int failures = 0;

    static void check(String what, boolean ok) {
        System.out.println((ok ? "  ok   " : "  FAIL ") + what);
        if (!ok) {
            failures++;
        }
    }

    /**
     * Run one check, counting a thrown exception as a failure.
     *
     * A broken build makes the very field this probe is about read back as
     * null, so a later case that dereferences it (iterating the set) throws out
     * of `main` and every remaining check goes unprinted — leaving the operator
     * with a partial list and no verdict line. Each case reports on its own.
     */
    static void checking(String what, java.util.concurrent.Callable<Boolean> body) {
        try {
            check(what, Boolean.TRUE.equals(body.call()));
        } catch (Throwable t) {
            check(what + " [threw " + t.getClass().getName() + "]", false);
        }
    }

    public static void main(String[] args) {
        // 1. A field read through a native, before and after one lock.
        Set<String> s = Collections.synchronizedSet(new LinkedHashSet<>());
        checking("iterator() before any lock", () -> s.iterator() != null);
        synchronized (s) {
            // deliberately empty: monitorenter + monitorexit is the whole test
        }
        checking("iterator() after one lock/unlock", () -> s.iterator() != null);

        // 2. Repeated and nested locking: every release level must carry the
        //    quartet, not only the last.
        Set<String> nested = Collections.synchronizedSet(new LinkedHashSet<>());
        for (int i = 0; i < 3; i++) {
            synchronized (nested) {
                synchronized (nested) {
                    // recursive thin lock
                }
            }
        }
        checking("iterator() after nested locks", () -> nested.iterator() != null);

        // 3. The JDK's own synchronized wrappers lock internally, so any
        //    Object-level call is enough to trip it without an explicit
        //    `synchronized` in this file.
        Set<String> viaToString = Collections.synchronizedSet(new LinkedHashSet<>());
        viaToString.toString();
        checking("iterator() after toString()", () -> viaToString.iterator() != null);

        Collection<String> viaHash = Collections.synchronizedCollection(new ArrayList<>());
        viaHash.hashCode();
        checking("iterator() after hashCode()", () -> viaHash.iterator() != null);

        // 4. Contents must survive too, not just the reference.
        Set<String> withData = Collections.synchronizedSet(new LinkedHashSet<>());
        withData.add("alpha");
        withData.add("beta");
        synchronized (withData) {
            // wipe point
        }
        checking("elements survive a lock", () -> {
            List<String> seen = new ArrayList<>();
            for (String v : withData) {
                seen.add(v);
            }
            return seen.equals(List.of("alpha", "beta"));
        });

        // 5. The unmodifiable-over-synchronized nesting that JUnit's
        //    AbstractTestDescriptor.getChildren() builds, which is how this
        //    defect failed every Spring Boot test class at discovery.
        Set<String> inner = Collections.synchronizedSet(new LinkedHashSet<>());
        inner.toString();
        Set<String> outer = Collections.unmodifiableSet(inner);
        checking("unmodifiableSet(synchronizedSet(...)).iterator()", () -> outer.iterator() != null);

        if (failures == 0) {
            System.out.println("PROBE-OK");
        } else {
            System.out.println("PROBE-FAILURES=" + failures);
        }
    }
}
