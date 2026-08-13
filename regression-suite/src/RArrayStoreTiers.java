import java.util.ArrayList;
import java.util.List;

/**
 * Regression: JVMS §aastore covariance (ArrayStoreException) must give the SAME
 * answer in the interpreter and in JIT-compiled code.
 *
 * <h2>Why a two-tier fixture</h2>
 *
 * The defect this fixture exists for was tier-dependent: the interpreter's
 * {@code aastore} enforced the store check and the x64 JIT's inline lowering did
 * not, so an illegal store threw for the first ~500 executions of a method and
 * then silently succeeded once the method tiered up. A fixture that performs
 * each store ONCE measures only the interpreter and cannot see that at all.
 *
 * So every shape below is executed in a loop and its answer is recorded at
 * three points: the FIRST iteration (cold / interpreted), every iteration (to
 * catch the exact iteration the answer moves), and the LAST iteration (hot /
 * compiled). A shape passes only when cold == hot == the HotSpot oracle value.
 * "Agreeing across tiers" is itself the property under test, so a disagreement
 * is reported with the iteration index at which it appeared.
 *
 * <h2>The tier-invariant observable is the exception KIND, not its message</h2>
 *
 * This fixture originally compared the full exception string across tiers and
 * went RED ON HOTSPOT: {@code s03} reported
 * {@code cold=[ArrayStoreException:java.lang.String]} and
 * {@code hot=[ArrayStoreException:null]}, moving at i=1375. That is HotSpot's
 * {@code -XX:+OmitStackTraceInFastThrow} (default ON): once an implicit
 * exception — ASE, NPE, AIOOBE, CCE — is thrown often enough from a COMPILED
 * site, HotSpot swaps in a preallocated instance with no message and no stack
 * trace. Running with {@code -XX:-OmitStackTraceInFastThrow} passes. It fires
 * per-site and only for some sites, so it looks exactly like a nondeterministic
 * VM bug if you have not seen it before.
 *
 * The consequence for this fixture: the message is NOT a tier-invariant, so
 * asserting it across tiers measures the oracle's own optimizer. The
 * tier-invariant property — and the one the defect actually breaks — is whether
 * the store is REFUSED, i.e. the exception kind. Messages are therefore checked
 * only on the COLD (interpreted) answer, where both VMs are deterministic.
 *
 * <h2>Iteration count</h2>
 *
 * {@code ITERS = 3000}. The C1 invocation threshold is 500
 * ({@code jit/src/tiered.rs}, {@code CompilationPolicy::default}, overridable
 * with {@code CRATONVM_TIER_C1_THRESHOLD}). Crossing 500 only ENQUEUES the
 * method on the background compile worker; the compiled entry is installed some
 * time after that, so a fixture that stops at, say, 600 can finish before the
 * compiled code is ever entered and read green on a broken VM. 3000 gives 2500
 * iterations of margin past the threshold — and is empirically enough for the
 * oracle's own compiled tier to engage, which the i=1375 transition above
 * demonstrates. If a MOVED@ number is ever close to ITERS, raise ITERS rather
 * than trusting the result.
 *
 * <h2>Run it BOTH ways</h2>
 *
 * Run once normally and once with {@code --nojit}. {@code --nojit} pins every
 * shape to the interpreter, so it isolates which tier a divergence lives in:
 * <ul>
 *   <li>red without {@code --nojit}, green with it → the JIT tier is wrong;</li>
 *   <li>red both ways → the shared check is wrong;</li>
 *   <li>green both ways → pass.</li>
 * </ul>
 * The two runs are not redundant: a single green run cannot distinguish "both
 * tiers are right" from "the JIT never engaged".
 *
 * <h2>Oracle</h2>
 *
 * Every expected value below was MEASURED on HotSpot
 * (openjdk 25.0.3 2026-04-21 LTS, Microsoft build 25.0.3+9-LTS), not recalled.
 */
public class RArrayStoreTiers {

    static final int ITERS = 3000;

    static final List<String> DIVERGENCES = new ArrayList<>();
    static int checks = 0;

    // ------------------------------------------------------------------
    // Store sites. Each shape lives in its OWN method so each gets its own
    // compiled site; a single shared helper would give every shape one
    // megamorphic site and would measure that instead of the store check.
    //
    // Operands are static fields read at their widest static type, so javac
    // must emit a real `aastore` against a runtime component type it cannot
    // prove — which is exactly the covariant shape the check governs. A local
    // `new String[1]` with a literal store could be folded or narrowed.
    // ------------------------------------------------------------------

    static final Object[] STRINGS_AS_OBJECTS = new String[1];
    static final Object[] TRUE_OBJECTS = new Object[1];
    static final Comparable<?>[] COMPARABLES = new Comparable<?>[1];
    static final Object[] COMPARABLES_AS_OBJECTS = COMPARABLES;
    static final Object[] RUNNABLES = new Runnable[1];
    static final Object[] STRING_ARRAYS = new String[1][];
    static final Object[][] OBJECT_ARRAYS = new Object[1][];
    static final Number[] INTEGERS_AS_NUMBERS = new Integer[1];

    static final Object AN_INTEGER = Integer.valueOf(1);
    static final Object A_STRING = "s";
    static final Object A_PLAIN_OBJECT = new Object();
    static final Object A_DOUBLE = Double.valueOf(1.0);
    static final Object AN_INTEGER_ARRAY = new Integer[1];
    static final Object A_STRING_ARRAY = new String[1];
    static final Object A_LAMBDA = (Runnable) () -> { };

    // MUST throw ArrayStoreException
    static void s01() { STRINGS_AS_OBJECTS[0] = AN_INTEGER; }
    static void s02() { COMPARABLES_AS_OBJECTS[0] = A_PLAIN_OBJECT; }
    static void s03() { RUNNABLES[0] = A_STRING; }
    static void s04() { STRING_ARRAYS[0] = AN_INTEGER_ARRAY; }
    static void s05() { INTEGERS_AS_NUMBERS[0] = (Number) A_DOUBLE; }

    // MUST succeed
    static void s06() { STRINGS_AS_OBJECTS[0] = A_STRING; }
    static void s07() { TRUE_OBJECTS[0] = AN_INTEGER; }
    static void s08() { COMPARABLES_AS_OBJECTS[0] = AN_INTEGER; }   // Integer IS Comparable
    static void s09() { RUNNABLES[0] = A_LAMBDA; }
    static void s10() { STRING_ARRAYS[0] = A_STRING_ARRAY; }
    static void s11() { OBJECT_ARRAYS[0] = (Object[]) A_STRING_ARRAY; }
    static void s12() { INTEGERS_AS_NUMBERS[0] = (Number) AN_INTEGER; }
    static void s13() { STRINGS_AS_OBJECTS[0] = null; }             // null: never checked

    // Exception PRECEDENCE: NPE and AIOOBE both precede the store check.
    static Object[] NULL_ARRAY = null;
    static void s14() { NULL_ARRAY[0] = AN_INTEGER; }
    static void s15() { STRINGS_AS_OBJECTS[5] = AN_INTEGER; }

    // Primitive contrast: no reference store check is possible here at all.
    static final int[] INTS = new int[1];
    static void s16() { INTS[0] = 42; }

    static void run(int id) {
        switch (id) {
            case 1: s01(); break; case 2: s02(); break; case 3: s03(); break;
            case 4: s04(); break; case 5: s05(); break; case 6: s06(); break;
            case 7: s07(); break; case 8: s08(); break; case 9: s09(); break;
            case 10: s10(); break; case 11: s11(); break; case 12: s12(); break;
            case 13: s13(); break; case 14: s14(); break; case 15: s15(); break;
            case 16: s16(); break;
            default: throw new IllegalArgumentException("id " + id);
        }
    }

    /**
     * The TIER-INVARIANT observable: which exception kind this store site
     * produces, or "no-throw". Deliberately excludes the message — see the
     * class comment on OmitStackTraceInFastThrow.
     */
    static String observeKind(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (ArrayStoreException e) {
            return "ArrayStoreException";
        } catch (NullPointerException e) {
            return "NullPointerException";
        } catch (ArrayIndexOutOfBoundsException e) {
            return "ArrayIndexOutOfBoundsException";
        } catch (Throwable t) {
            return "UNEXPECTED:" + t.getClass().getName();
        }
    }

    /** Cold-only observable: kind PLUS message. Valid interpreted, not hot. */
    static String observeWithMessage(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + ":" + t.getMessage();
        }
    }

    static final String[] NAMES = {
        "", // 1-based
        "s01 String[] as Object[] <- Integer",
        "s02 Comparable[] as Object[] <- Object",
        "s03 Runnable[] as Object[] <- String",
        "s04 String[][] as Object[] <- Integer[]",
        "s05 Integer[] as Number[] <- Double",
        "s06 String[] as Object[] <- String",
        "s07 Object[] <- Integer",
        "s08 Comparable[] as Object[] <- Integer",
        "s09 Runnable[] as Object[] <- lambda",
        "s10 String[][] as Object[] <- String[]",
        "s11 Object[][] <- String[]",
        "s12 Integer[] as Number[] <- Integer",
        "s13 String[] as Object[] <- null",
        "s14 null array <- Integer",
        "s15 String[] as Object[] index 5 <- Integer",
        "s16 int[] <- 42",
    };

    /** Tier-invariant expectation. MEASURED on HotSpot 25.0.3, not recalled. */
    static final String[] EXPECTED_KIND = {
        "", // 1-based
        "ArrayStoreException", "ArrayStoreException", "ArrayStoreException",
        "ArrayStoreException", "ArrayStoreException",
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw",
        "NullPointerException", "ArrayIndexOutOfBoundsException", "no-throw",
    };

    /**
     * Cold-only message expectation. MEASURED on HotSpot 25.0.3 in the
     * interpreted tier. `null` entries mean "do not assert the message" —
     * used for the NPE, whose helpful-NPE text names a local slot and is not
     * a property this fixture is about.
     */
    static final String[] EXPECTED_COLD = {
        "", // 1-based
        "java.lang.ArrayStoreException:java.lang.Integer",
        "java.lang.ArrayStoreException:java.lang.Object",
        "java.lang.ArrayStoreException:java.lang.String",
        "java.lang.ArrayStoreException:[Ljava.lang.Integer;",
        "java.lang.ArrayStoreException:java.lang.Double",
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw",
        null,
        "java.lang.ArrayIndexOutOfBoundsException:Index 5 out of bounds for length 1",
        "no-throw",
    };

    public static void main(String[] args) {
        System.out.println("RArrayStoreTiers ITERS=" + ITERS
            + "  (C1 threshold 500; run this both with and without --nojit)");

        // Pass 1: cold messages, one execution per site, guaranteed interpreted
        // because nothing has run yet.
        for (int id = 1; id <= 16; id++) {
            String want = EXPECTED_COLD[id];
            if (want == null) continue;
            String got = observeWithMessage(id);
            checks++;
            if (!want.equals(got)) {
                DIVERGENCES.add(NAMES[id] + " COLD-MESSAGE: want=[" + want + "] got=[" + got + "]");
            }
        }

        // Pass 2: tier parity on the exception KIND.
        for (int id = 1; id <= 16; id++) {
            String cold = null, hot = null, moved = null;
            int movedAt = -1;

            for (int i = 0; i < ITERS; i++) {
                String a = observeKind(id);
                if (i == 0) {
                    cold = a;
                } else if (movedAt < 0 && !a.equals(cold)) {
                    movedAt = i;
                    moved = a;
                }
                hot = a;
            }

            String name = NAMES[id];
            String want = EXPECTED_KIND[id];

            checks++;
            if (!want.equals(cold)) {
                DIVERGENCES.add(name + " COLD: want=[" + want + "] got=[" + cold + "]");
            }
            checks++;
            if (!want.equals(hot)) {
                DIVERGENCES.add(name + " HOT: want=[" + want + "] got=[" + hot + "]");
            }
            // Reported separately because it names the tier transition directly:
            // this is the assertion the inline-aastore defect trips.
            checks++;
            if (movedAt >= 0) {
                DIVERGENCES.add(name + " TIER-SPLIT at i=" + movedAt
                    + ": cold=[" + cold + "] became=[" + moved + "] final=[" + hot + "]");
            }

            System.out.printf("%-46s cold=[%s] hot=[%s]%s%n",
                name, cold, hot, movedAt >= 0 ? "  MOVED@" + movedAt : "");
        }

        if (!DIVERGENCES.isEmpty()) {
            for (String d : DIVERGENCES) {
                System.out.println("DIVERGENCE " + d);
            }
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RArrayStoreTiers (" + checks + " checks)");
    }
}
