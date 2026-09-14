import java.io.Serializable;
import java.lang.annotation.Annotation;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;

/**
 * Regression: JVMS §aastore against an array whose component type is an
 * INTERFACE.
 *
 * <h2>Why this exists beside {@code RArrayStoreTiers}</h2>
 *
 * {@code RArrayStoreTiers} carries four interface shapes ({@code s02}, {@code s03},
 * {@code s08}, {@code s09}) as part of a broader tier-parity table. This fixture
 * is the interface component type on its own, widened, because the defect it
 * covers was a single predicate arm that answered for the WHOLE family without
 * looking at the value:
 *
 * <pre>
 *   if component.is_interface() { return true; }   // vm/.../typecheck.rs
 * </pre>
 *
 * Four rows are not enough to distinguish "the check works" from "the check is
 * absent", because a blanket allow gets every LEGAL row right for free. So the
 * table below is deliberately balanced: each legal store is paired with an
 * illegal store into the SAME array, and a run that reports every legal row
 * green and every illegal row green is reporting that nothing is being checked.
 *
 * <h2>Two independent fail-open arms produced the same symptom</h2>
 *
 * The blanket above was not the only one. {@code ClassId(0)} in that VM is
 * {@code java/lang/Object} (it is the first class loaded and ids are dense from
 * zero), and a second arm returned {@code true} for any value whose class id was
 * zero, described in-comment as "unknown/synthetic class id". That arm sits
 * ABOVE the interface arm, so it — not the blanket — is what admitted
 * {@code Comparable[] <- Object}. Hence the {@code <- Object} rows here are
 * scored as their own group: they and the {@code <- String}/{@code <- Integer}
 * rows fail for different reasons and can be fixed independently.
 *
 * <h2>What must keep passing</h2>
 *
 * The fail-open populations are real and this fixture asserts them rather than
 * leaving them to be rediscovered: a dynamic proxy stores into an array of an
 * interface it was created with, and an annotation proxy stores into an
 * {@code Annotation[]}. A fix that turns those red has replaced one wrong
 * answer with another.
 *
 * <h2>Oracle</h2>
 *
 * Every expectation below was MEASURED on HotSpot
 * (openjdk 25.0.3 2026-04-21 LTS, Microsoft build 25.0.3+9-LTS), not recalled —
 * including the three that contradict a natural guess:
 * {@code Serializable[] <- Object} and {@code Cloneable[] <- Object} THROW (only
 * {@code Object[]} accepts every reference; {@code Serializable} and
 * {@code Cloneable} are ordinary interfaces for this purpose), and
 * {@code Cloneable[] <- Integer} throws while {@code Cloneable[] <- Integer[]}
 * does not (arrays implement {@code Cloneable}; {@code Integer} does not).
 *
 * <h2>Run it both ways</h2>
 *
 * Once normally and once with {@code --nojit}. Red under {@code --nojit} is the
 * shared predicate; red only without it is the compiled tier. Messages are
 * asserted only on the cold pass — see
 * {@code docs/known-issues/jdk-only/W7-40-tier-parity-fixtures-and-fast-throw.md}
 * for why a message is not a tier-invariant.
 *
 * <h2>Reporting dialect — why every line is {@code CK }/{@code PASS }-prefixed</h2>
 *
 * {@code run.sh} reduces both VMs' output to its {@code ^PASS }/{@code ^CK }
 * lines before diffing them ({@code extract()} in
 * {@code regression-suite/harness-guard.sh}). Until 2026-08-13 this fixture
 * printed its ITERS header, all twenty-seven per-row {@code cold=/hot=} lines
 * and — worst of the three — the BALANCE line on no prefix, so 29 of 30 lines
 * were deleted and the harness compared the constant banner alone. The balance
 * line is the one that separates "the check works" from "the check is absent",
 * which is the whole reason this fixture exists beside
 * {@code RArrayStoreTiers}; discarding it left a 108-check vector whose extract
 * was one constant string. Every row now goes out as
 * {@code CK RArrayStoreInterfaces <row> cold=[..] hot=[..] moved=<i>} carrying
 * the VM's OWN answers, so the two VMs diff their answers against each other.
 * See
 * {@code docs/known-issues/jdk-only/W8-E15-1-the-fourth-broken-oracle-the-unscheduled-vector-and-the-reach-ratchet.md}
 * §NOM-2, and W8-E9-1 §1 for why {@code fails} and {@code checks} are on
 * SEPARATE lines.
 *
 * <p>The two rows whose message is not a stable property ({@code s12}'s
 * generated proxy name) are deliberately not published either: an unasserted
 * value on a {@code CK } line becomes an assertion by way of the cross-VM diff.
 */
public class RArrayStoreInterfaces {

    static final int ITERS = 3000;

    static final List<String> DIVERGENCES = new ArrayList<>();
    static int checks = 0;

    // ------------------------------------------------------------------
    // Types. `Marker` and friends are declared here rather than reused from
    // the JDK so the VM's name-based synthetic fallbacks (which match on
    // "List"/"Set"/"Collection"/"$Proxy"/... substrings) cannot answer for
    // them by accident and make a row pass without the hierarchy being read.
    // ------------------------------------------------------------------

    interface Marker {}
    interface SuperI {}
    interface SubI extends SuperI {}
    static class Impl implements Marker {}
    static class Sub extends Impl {}
    static class DeepImpl implements SubI {}
    static class Unrelated {}
    static class MyComparable implements Comparable<MyComparable> {
        public int compareTo(MyComparable o) { return 0; }
    }

    @Retention(RetentionPolicy.RUNTIME) @interface Ann {}
    @Ann static class Annotated {}

    // Arrays are held at their WIDEST static type so javac must emit a real
    // `aastore` against a runtime component type it cannot prove. A field typed
    // `Marker[]` with a `Marker`-typed value gives javac enough to elide the
    // question entirely, which measures nothing.
    static final Object[] MARKERS      = new Marker[1];
    static final Object[] SUPER_IS     = new SuperI[1];
    static final Object[] COMPARABLES  = new Comparable<?>[1];
    static final Object[] SERIALIZABLES= new Serializable[1];
    static final Object[] CLONEABLES   = new Cloneable[1];
    static final Object[] MAP_ENTRIES  = new Map.Entry<?, ?>[1];
    static final Object[] CHAR_SEQS    = new CharSequence[1];
    static final Object[] RUNNABLES    = new Runnable[1];
    static final Object[] ANNOTATIONS  = new Annotation[1];
    static final Object[] MARKER_ARRAYS   = new Marker[1][];
    static final Object[] RUNNABLE_ARRAYS = new Runnable[1][];

    static final Object AN_INTEGER   = Integer.valueOf(1);
    static final Object A_STRING     = "s";
    static final Object A_PLAIN_OBJ  = new Object();
    static final Object AN_IMPL      = new Impl();
    static final Object A_SUB        = new Sub();
    static final Object A_DEEP       = new DeepImpl();
    static final Object AN_UNRELATED = new Unrelated();
    static final Object A_MYCOMP     = new MyComparable();
    static final Object A_LAMBDA     = (Runnable) () -> { };
    static final Object AN_INT_ARRAY = new Integer[1];
    static final Object A_STR_ARRAY  = new String[1];
    static final Object A_MARKER_ARR = new Marker[1];
    static final Object AN_ANNOTATION =
        Annotated.class.getAnnotation(Ann.class);           // an annotation proxy
    static final Object A_DYN_PROXY = Proxy.newProxyInstance(
        RArrayStoreInterfaces.class.getClassLoader(),
        new Class<?>[]{ Marker.class },
        (p, m, args) -> null);                              // a dynamic proxy

    // --- ILLEGAL: must throw ArrayStoreException -----------------------
    static void s01() { MARKERS[0]       = A_STRING; }
    static void s02() { MARKERS[0]       = AN_UNRELATED; }
    static void s03() { SUPER_IS[0]      = AN_IMPL; }        // Impl has no SubI
    static void s04() { COMPARABLES[0]   = A_PLAIN_OBJ; }
    static void s05() { RUNNABLES[0]     = A_STRING; }
    static void s06() { CHAR_SEQS[0]     = AN_INTEGER; }
    static void s07() { MAP_ENTRIES[0]   = AN_INTEGER; }
    static void s08() { SERIALIZABLES[0] = A_PLAIN_OBJ; }
    static void s09() { CLONEABLES[0]    = A_PLAIN_OBJ; }
    static void s10() { CLONEABLES[0]    = AN_INTEGER; }     // Integer isn't Cloneable
    static void s11() { RUNNABLE_ARRAYS[0] = A_STR_ARRAY; }
    static void s12() { RUNNABLES[0]     = A_DYN_PROXY; }    // proxy of Marker, not Runnable

    // --- LEGAL: must succeed ------------------------------------------
    static void s13() { MARKERS[0]       = AN_IMPL; }        // direct implement
    static void s14() { MARKERS[0]       = A_SUB; }          // implement via superclass
    static void s15() { SUPER_IS[0]      = A_DEEP; }         // via super-interface
    static void s16() { COMPARABLES[0]   = AN_INTEGER; }     // Integer IMPLEMENTS Comparable
    static void s17() { COMPARABLES[0]   = A_STRING; }
    static void s18() { COMPARABLES[0]   = A_MYCOMP; }       // user class, no name table
    static void s19() { RUNNABLES[0]     = A_LAMBDA; }
    static void s20() { CHAR_SEQS[0]     = A_STRING; }
    static void s21() { SERIALIZABLES[0] = AN_INTEGER; }
    static void s22() { SERIALIZABLES[0] = AN_INT_ARRAY; }   // arrays are Serializable
    static void s23() { CLONEABLES[0]    = AN_INT_ARRAY; }   // arrays are Cloneable
    static void s24() { MARKER_ARRAYS[0] = A_MARKER_ARR; }
    static void s25() { MARKERS[0]       = A_DYN_PROXY; }    // proxy OF Marker
    static void s26() { ANNOTATIONS[0]   = AN_ANNOTATION; }  // annotation proxy
    static void s27() { MARKERS[0]       = null; }           // null is never checked

    static void run(int id) {
        switch (id) {
            case  1: s01(); break; case  2: s02(); break; case  3: s03(); break;
            case  4: s04(); break; case  5: s05(); break; case  6: s06(); break;
            case  7: s07(); break; case  8: s08(); break; case  9: s09(); break;
            case 10: s10(); break; case 11: s11(); break; case 12: s12(); break;
            case 13: s13(); break; case 14: s14(); break; case 15: s15(); break;
            case 16: s16(); break; case 17: s17(); break; case 18: s18(); break;
            case 19: s19(); break; case 20: s20(); break; case 21: s21(); break;
            case 22: s22(); break; case 23: s23(); break; case 24: s24(); break;
            case 25: s25(); break; case 26: s26(); break; case 27: s27(); break;
            default: throw new IllegalArgumentException("id " + id);
        }
    }

    static final int N = 27;

    static final String[] NAMES = {
        "", // 1-based
        "s01 Marker[]        <- String",
        "s02 Marker[]        <- Unrelated",
        "s03 SuperI[]        <- Impl",
        "s04 Comparable[]    <- Object",
        "s05 Runnable[]      <- String",
        "s06 CharSequence[]  <- Integer",
        "s07 Map.Entry[]     <- Integer",
        "s08 Serializable[]  <- Object",
        "s09 Cloneable[]     <- Object",
        "s10 Cloneable[]     <- Integer",
        "s11 Runnable[][]    <- String[]",
        "s12 Runnable[]      <- Proxy(Marker)",
        "s13 Marker[]        <- Impl",
        "s14 Marker[]        <- Sub",
        "s15 SuperI[]        <- DeepImpl",
        "s16 Comparable[]    <- Integer",
        "s17 Comparable[]    <- String",
        "s18 Comparable[]    <- MyComparable",
        "s19 Runnable[]      <- lambda",
        "s20 CharSequence[]  <- String",
        "s21 Serializable[]  <- Integer",
        "s22 Serializable[]  <- Integer[]",
        "s23 Cloneable[]     <- Integer[]",
        "s24 Marker[][]      <- Marker[]",
        "s25 Marker[]        <- Proxy(Marker)",
        "s26 Annotation[]    <- annotation proxy",
        "s27 Marker[]        <- null",
    };

    /** Tier-invariant expectation. MEASURED on HotSpot 25.0.3, not recalled. */
    static final String[] EXPECTED_KIND = {
        "", // 1-based
        "ArrayStoreException", "ArrayStoreException", "ArrayStoreException",
        "ArrayStoreException", "ArrayStoreException", "ArrayStoreException",
        "ArrayStoreException", "ArrayStoreException", "ArrayStoreException",
        "ArrayStoreException", "ArrayStoreException", "ArrayStoreException",
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw",
    };

    static String observeKind(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (ArrayStoreException e) {
            return "ArrayStoreException";
        } catch (Throwable t) {
            return "UNEXPECTED:" + t.getClass().getName();
        }
    }

    /**
     * Cold-only observable: kind PLUS message. Only valid interpreted — HotSpot's
     * {@code -XX:+OmitStackTraceInFastThrow} drops the message once an implicit
     * exception has been thrown often enough from a COMPILED site.
     *
     * The dynamic-proxy row's message names a generated class whose number is not
     * stable, so its message is not asserted (see {@code COLD_SKIP}).
     */
    static String observeWithMessage(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + ":" + t.getMessage();
        }
    }

    /** Rows whose cold MESSAGE is not a stable property. Kind is still asserted. */
    static boolean coldSkip(int id) { return id == 12; }

    static final String[] EXPECTED_COLD = {
        "", // 1-based
        "java.lang.ArrayStoreException:java.lang.String",
        "java.lang.ArrayStoreException:RArrayStoreInterfaces$Unrelated",
        "java.lang.ArrayStoreException:RArrayStoreInterfaces$Impl",
        "java.lang.ArrayStoreException:java.lang.Object",
        "java.lang.ArrayStoreException:java.lang.String",
        "java.lang.ArrayStoreException:java.lang.Integer",
        "java.lang.ArrayStoreException:java.lang.Integer",
        "java.lang.ArrayStoreException:java.lang.Object",
        "java.lang.ArrayStoreException:java.lang.Object",
        "java.lang.ArrayStoreException:java.lang.Integer",
        "java.lang.ArrayStoreException:[Ljava.lang.String;",
        null, // s12 — generated proxy name is not stable
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw", "no-throw", "no-throw", "no-throw",
        "no-throw", "no-throw", "no-throw",
    };

    /**
     * The one evidence funnel. Prints UNCONDITIONALLY and prints the VM's own
     * answer, so a row's value reaches the cross-VM diff whether or not this
     * file's table agrees with it.
     */
    static void ck(String row, String fields) {
        System.out.printf("CK RArrayStoreInterfaces %-42s %s%n", row, fields);
    }

    /**
     * Record a divergence AND make it visible on a kept prefix. The old code
     * printed these as {@code DIVERGENCE <text>}, which extract() deletes — so
     * on a red run the harness could see that something failed but not what.
     */
    static void diverge(String detail) {
        DIVERGENCES.add(detail);
        System.out.println("CK RArrayStoreInterfaces FAILED " + detail);
    }

    public static void main(String[] args) {
        // ITERS is a real parameter of the measurement, not decoration: read
        // together with a moved= index it is what says whether the compiled
        // tier was reached at all. It goes into the diff for that reason.
        System.out.println("CK RArrayStoreInterfaces ITERS=" + ITERS);

        // Pass 1: cold. One execution per site, guaranteed interpreted because
        // nothing has run yet — the only tier where the message is meaningful.
        for (int id = 1; id <= N; id++) {
            if (coldSkip(id)) continue;
            String want = EXPECTED_COLD[id];
            // null = "do not assert the message", and therefore also "do not
            // publish it".
            if (want == null) continue;
            String got = observeWithMessage(id);
            checks++;
            ck(NAMES[id], "coldmsg=[" + got + "]");
            if (!want.equals(got)) {
                diverge(NAMES[id] + " COLD-MESSAGE: want=[" + want + "] got=[" + got + "]");
            }
        }

        // Pass 2: tier parity on the exception KIND.
        int illegalRefused = 0, legalAdmitted = 0;
        for (int id = 1; id <= N; id++) {
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

            // Evidence FIRST, verdicts after, so a FAILED line always follows
            // the values it is about. `moved` is published on every row and not
            // just on a transition: an absent field is not evidence, and the
            // iteration at which an answer moved is precisely what a
            // tier-dependent store check shows up as.
            ck(name, "cold=[" + cold + "] hot=[" + hot + "] moved=" + movedAt);

            checks++;
            if (!want.equals(cold)) {
                diverge(name + " COLD: want=[" + want + "] got=[" + cold + "]");
            }
            checks++;
            if (!want.equals(hot)) {
                diverge(name + " HOT: want=[" + want + "] got=[" + hot + "]");
            }
            checks++;
            if (movedAt >= 0) {
                diverge(name + " TIER-SPLIT at i=" + movedAt
                    + ": cold=[" + cold + "] became=[" + moved + "] final=[" + hot + "]");
            }

            if ("ArrayStoreException".equals(want) && "ArrayStoreException".equals(hot)) {
                illegalRefused++;
            }
            if ("no-throw".equals(want) && "no-throw".equals(hot)) {
                legalAdmitted++;
            }
        }

        // The balance assertion, and the single most important line this
        // fixture prints: a predicate that has degenerated to "allow
        // everything" satisfies every LEGAL row above and nothing else catches
        // it, because the legal rows are the majority. It was on a deleted
        // prefix, i.e. the harness could not see the one number that separates
        // "checked" from "not checked".
        //
        // The DENOMINATORS are counted off EXPECTED_KIND rather than written as
        // literals: the old line said "/12" and "/15" by hand, so adding a row
        // to the table would have silently made the ratio wrong rather than the
        // run red.
        int illegalTotal = 0, legalTotal = 0;
        for (int id = 1; id <= N; id++) {
            if ("ArrayStoreException".equals(EXPECTED_KIND[id])) illegalTotal++;
            if ("no-throw".equals(EXPECTED_KIND[id])) legalTotal++;
        }
        System.out.println("CK RArrayStoreInterfaces illegalRefused=" + illegalRefused + "/" + illegalTotal);
        System.out.println("CK RArrayStoreInterfaces legalAdmitted=" + legalAdmitted + "/" + legalTotal);
        checks++;
        if (illegalRefused == 0) {
            diverge("DEGENERATE: not one illegal interface store was refused - "
                + "the component-type check is absent, not merely wrong");
        }

        // SEPARATE lines, and in this order. harness_check_count does
        // `sub(/^.*checks=/, ""); print`, so `checks=108 fails=0` would publish
        // the "count" `108 fails=0` and the guard that reads it would no-op
        // inside its own 2>/dev/null. One value per line.
        System.out.println("CK RArrayStoreInterfaces fails=" + DIVERGENCES.size());
        System.out.println("CK RArrayStoreInterfaces checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RArrayStoreInterfaces (" + checks + " checks)");
    }
}
