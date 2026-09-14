import java.lang.reflect.Array;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.LinkedHashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Set;

/**
 * Regression: the array-store check made by a LIBRARY METHOD, not by the
 * {@code aastore} opcode.
 *
 * <h2>Why this exists beside {@code RArrayStoreTiers} / {@code RArrayStoreInterfaces}</h2>
 *
 * Those two ask the opcode, in both tiers. Neither can reach the other doors,
 * and there are five: {@code Arrays.fill(Object[], Object)},
 * {@code Arrays.copyOf(T[], int, Class)}, {@code Collection.toArray(T[])} (two
 * routes, two different messages), {@code System.arraycopy} and
 * {@code java.lang.reflect.Array.set}. Each is implemented in this VM as a
 * NATIVE that performs the stores itself, so each carries its own copy of the
 * "may this be stored here" question — and a copy is a place the answer can be
 * different.
 *
 * <p>It was. MEASURED, hibernate-reactive complete suite 2026-08-29: nine
 * embeddable/embedded-id test classes died with
 *
 * <pre>
 *   java.lang.ArrayStoreException: org.hibernate.sql.results.graph.Initializer
 *       at ...EmbeddableInitializerImpl.fill(EmbeddableInitializerImpl.java:197)
 * </pre>
 *
 * whose body is one line — {@code Arrays.fill(initializers, Initializer.EMPTY_ARRAY)},
 * filling an {@code Initializer[][]} with an {@code Initializer[]}. The store is
 * legal, the opcode admits it, and {@code Arrays.fill}'s native refused it.
 *
 * <h2>The message was the tell, and it is asserted here for that reason</h2>
 *
 * {@code Initializer} is an INTERFACE. No instance can ever have an interface as
 * its class, so an {@code ArrayStoreException} naming one cannot be reporting a
 * value's type — it was reporting a COMPONENT. On a reference array this VM's
 * heap header holds the component class rather than the array's own type, which
 * makes {@code class_id_of_object(v)} off by exactly one array dimension for an
 * array-valued element and right for everything else. Every row below whose
 * value is an ARRAY exists to pin that dimension: a fix that repairs the
 * refusal but keeps naming the component leaves those rows green on kind and
 * red on message, which is precisely the distinction this fixture is built to
 * make. The refusal and the message were separate defects with one cause, and
 * they were fixed a day apart.
 *
 * <h2>Balance</h2>
 *
 * Each door carries LEGAL and ILLEGAL rows, and both counts are published. A
 * store check that has degenerated to "allow everything" — which is what every
 * one of these natives did before it was given a check at all — satisfies every
 * legal row for free, and the legal rows are the majority.
 *
 * <h2>Oracle</h2>
 *
 * Every expectation below was MEASURED on HotSpot (openjdk 25.0.3 2026-04-21
 * LTS, Microsoft build 25.0.3+9-LTS), one execution per shape, not recalled.
 * Three of them contradict a natural guess:
 *
 * <ul>
 *   <li>{@code aastore}'s message for an array VALUE is the dotted DESCRIPTOR
 *       ({@code [Ljava.lang.Integer;}), because that is
 *       {@code ObjArrayKlass::external_name()} — not the source form
 *       {@code java.lang.Integer[]}, which is what {@code System.arraycopy}'s
 *       own (quite different) sentence uses for the destination COMPONENT.</li>
 *   <li>{@code ArrayList.toArray(T[])} and {@code AbstractCollection.toArray(T[])}
 *       throw DIFFERENT text for the same narrowing, because the first copies
 *       through {@code System.arraycopy} and the second stores through
 *       {@code aastore} in its own loop.</li>
 *   <li>{@code Array.set} throws {@code IllegalArgumentException}, not
 *       {@code ArrayStoreException}, and its wording
 *       ({@code "array element type mismatch"}) is a fourth distinct string.</li>
 * </ul>
 *
 * <h2>Reporting dialect</h2>
 *
 * {@code run.sh} reduces both VMs' output to its {@code ^PASS }/{@code ^CK }
 * lines before diffing, so every row goes out {@code CK }-prefixed carrying
 * this VM's OWN answer — the two VMs diff their answers against each other, and
 * this file's table is a second, independent opinion rather than the only one.
 * See {@code RArrayStoreInterfaces} for the longer version of that note, and
 * {@code W8-E9-1} §1 for why {@code fails} and {@code checks} are on SEPARATE
 * lines.
 */
public class RArrayStoreLibrary {

    static final int ITERS = 3000;

    static final List<String> DIVERGENCES = new ArrayList<>();
    static int checks = 0;

    // Declared here rather than reused from the JDK so the VM's name-based
    // synthetic fallbacks (which match "List"/"Set"/"$Proxy"/... substrings)
    // cannot answer for them by accident and make a row pass without the
    // hierarchy being read.
    interface I {}
    static class Impl implements I {}
    static class Unrelated {}

    // ------------------------------------------------------------------
    // Rows. Each allocates its own arrays: a row is executed ITERS times and a
    // fill mutates what it is given.
    // ------------------------------------------------------------------

    // --- Arrays.fill(Object[], Object) --------------------------------
    // s01 is the hibernate-reactive shape reduced to its bones: an
    // interface-component two-dimensional array filled with a one-dimensional
    // array of the same component.
    static void s01() { Arrays.fill((Object[]) new I[3][], new I[0]); }
    static void s02() { Arrays.fill((Object[]) new String[3][], new String[0]); }
    // s03 is `sun.nio.cs.HKSCS$Encoder.initc2b`'s `Arrays.fill(c2b, C2B_UNMAPPABLE)`:
    // a PRIMITIVE-array component, which has no ordinary class id to compare
    // against. It broke every real Big5-HKSCS/MS950_HKSCS charset's static init.
    static void s03() { Arrays.fill((Object[]) new char[3][], new char[] { 'a' }); }
    static void s04() { Arrays.fill(new Object[3][], new I[0]); }
    static void s05() { Arrays.fill((Object[]) new I[3][][], new I[0][]); }
    static void s06() { Arrays.fill((Object[]) new I[3], new Impl()); }
    static void s07() { Arrays.fill((Object[]) new I[3], (Object) null); }
    static void s08() { Arrays.fill((Object[]) new I[3], new Object()); }
    static void s09() { Arrays.fill((Object[]) new String[3], Integer.valueOf(1)); }
    static void s10() { Arrays.fill((Object[]) new String[3][], new Integer[0]); }
    static void s11() { Arrays.fill((Object[]) new I[3], new Unrelated()); }

    // --- Arrays.copyOf(T[], int, Class) -------------------------------
    static void s12() {
        Object[] r = Arrays.copyOf(new Object[] { new String[0] }, 1, String[][].class);
        if (!(r instanceof String[][])) throw new IllegalStateException("copyOf lost the type");
    }
    static void s13() {
        Object[] r = Arrays.copyOf(new Object[] { new I[0] }, 1, I[][].class);
        if (!(r instanceof I[][])) throw new IllegalStateException("copyOf lost the type");
    }
    static void s14() {
        Object[] r = Arrays.copyOf(new Object[] { new Impl() }, 1, I[].class);
        if (!(r instanceof I[])) throw new IllegalStateException("copyOf lost the type");
    }
    static void s15() { Arrays.copyOf(new Object[] { Integer.valueOf(1) }, 1, String[].class); }

    // --- ArrayList.toArray(T[]) — the System.arraycopy route ----------
    static void s16() { List<I[]> l = new ArrayList<>(); l.add(new I[0]); l.toArray(new I[0][]); }
    static void s17() { List<I> l = new ArrayList<>(); l.add(new Impl()); l.toArray(new I[0]); }
    static void s18() {
        List<Object> l = new ArrayList<>(); l.add(Integer.valueOf(1)); l.toArray(new String[0]);
    }

    // --- AbstractCollection.toArray(T[]) — the aastore route ----------
    static void s19() { Set<I[]> s = new LinkedHashSet<>(); s.add(new I[0]); s.toArray(new I[0][]); }
    static void s20() { List<I> l = new LinkedList<>(); l.add(new Impl()); l.toArray(new I[0]); }
    static void s21() {
        Set<Object> s = new LinkedHashSet<>(); s.add(Integer.valueOf(1)); s.toArray(new String[0]);
    }

    // --- System.arraycopy ---------------------------------------------
    static void s22() { System.arraycopy(new I[][] { new I[0] }, 0, new I[1][], 0, 1); }
    static void s23() { System.arraycopy(new Object[] { Integer.valueOf(1) }, 0, new String[1], 0, 1); }

    // --- java.lang.reflect.Array.set ----------------------------------
    static void s24() { Array.set(new I[1][], 0, new I[0]); }
    static void s25() { Array.set(new I[1], 0, new Impl()); }
    static void s26() { Array.set(new String[1], 0, Integer.valueOf(1)); }

    // --- the opcode itself, as the CONTROL ----------------------------
    // Not coverage of `aastore` — that is RArrayStoreTiers' and
    // RArrayStoreInterfaces' job — but the reference answer the doors above are
    // supposed to agree with. A run where s27/s28 are right and s01/s10 are
    // wrong has already named the door.
    static void s27() { Object[] a = new I[1][]; a[0] = new I[0]; }
    static void s28() { Object[] a = new String[1][]; a[0] = new Integer[0]; }

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
            case 28: s28(); break;
            default: throw new IllegalArgumentException("id " + id);
        }
    }

    static final int N = 28;

    static final String[] NAMES = {
        "", // 1-based
        "s01 fill      I[][]      <- I[]",
        "s02 fill      String[][] <- String[]",
        "s03 fill      char[][]   <- char[]",
        "s04 fill      Object[][] <- I[]",
        "s05 fill      I[][][]    <- I[][]",
        "s06 fill      I[]        <- Impl",
        "s07 fill      I[]        <- null",
        "s08 fill      I[]        <- Object",
        "s09 fill      String[]   <- Integer",
        "s10 fill      String[][] <- Integer[]",
        "s11 fill      I[]        <- Unrelated",
        "s12 copyOf    String[][] <- String[]",
        "s13 copyOf    I[][]      <- I[]",
        "s14 copyOf    I[]        <- Impl",
        "s15 copyOf    String[]   <- Integer",
        "s16 alToArray I[][]      <- I[]",
        "s17 alToArray I[]        <- Impl",
        "s18 alToArray String[]   <- Integer",
        "s19 acToArray I[][]      <- I[]",
        "s20 acToArray I[]        <- Impl",
        "s21 acToArray String[]   <- Integer",
        "s22 arraycopy I[][]      <- I[][]",
        "s23 arraycopy String[]   <- Object[]{Integer}",
        "s24 Array.set I[][]      <- I[]",
        "s25 Array.set I[]        <- Impl",
        "s26 Array.set String[]   <- Integer",
        "s27 aastore   I[][]      <- I[]",
        "s28 aastore   String[][] <- Integer[]",
    };

    /** Tier-invariant expectation. MEASURED on HotSpot 25.0.3, not recalled. */
    static final String[] EXPECTED_KIND = {
        "", // 1-based
        "no-throw",                 // s01
        "no-throw",                 // s02
        "no-throw",                 // s03
        "no-throw",                 // s04
        "no-throw",                 // s05
        "no-throw",                 // s06
        "no-throw",                 // s07
        "ArrayStoreException",      // s08
        "ArrayStoreException",      // s09
        "ArrayStoreException",      // s10
        "ArrayStoreException",      // s11
        "no-throw",                 // s12
        "no-throw",                 // s13
        "no-throw",                 // s14
        "ArrayStoreException",      // s15
        "no-throw",                 // s16
        "no-throw",                 // s17
        "ArrayStoreException",      // s18
        "no-throw",                 // s19
        "no-throw",                 // s20
        "ArrayStoreException",      // s21
        "no-throw",                 // s22
        "ArrayStoreException",      // s23
        "no-throw",                 // s24
        "no-throw",                 // s25
        "IllegalArgumentException", // s26 — Array.set's own species
        "no-throw",                 // s27
        "ArrayStoreException",      // s28
    };

    static final String ARRAYCOPY_TO_STRING =
        "java.lang.ArrayStoreException:arraycopy: element type mismatch: can not cast one of "
            + "the elements of java.lang.Object[] to the type of the destination array, "
            + "java.lang.String";

    /**
     * Cold-only observable: kind PLUS message. Only valid interpreted —
     * HotSpot's {@code -XX:+OmitStackTraceInFastThrow} drops the message once an
     * implicit exception has been thrown often enough from a COMPILED site. See
     * {@code W7-40-tier-parity-fixtures-and-fast-throw.md}.
     */
    static final String[] EXPECTED_COLD = {
        "", // 1-based
        "no-throw",                                                   // s01
        "no-throw",                                                   // s02
        "no-throw",                                                   // s03
        "no-throw",                                                   // s04
        "no-throw",                                                   // s05
        "no-throw",                                                   // s06
        "no-throw",                                                   // s07
        "java.lang.ArrayStoreException:java.lang.Object",             // s08
        "java.lang.ArrayStoreException:java.lang.Integer",            // s09
        "java.lang.ArrayStoreException:[Ljava.lang.Integer;",         // s10
        "java.lang.ArrayStoreException:RArrayStoreLibrary$Unrelated", // s11
        "no-throw",                                                   // s12
        "no-throw",                                                   // s13
        "no-throw",                                                   // s14
        ARRAYCOPY_TO_STRING,                                          // s15
        "no-throw",                                                   // s16
        "no-throw",                                                   // s17
        ARRAYCOPY_TO_STRING,                                          // s18
        "no-throw",                                                   // s19
        "no-throw",                                                   // s20
        "java.lang.ArrayStoreException:java.lang.Integer",            // s21
        "no-throw",                                                   // s22
        ARRAYCOPY_TO_STRING,                                          // s23
        "no-throw",                                                   // s24
        "no-throw",                                                   // s25
        "java.lang.IllegalArgumentException:array element type mismatch", // s26
        "no-throw",                                                   // s27
        "java.lang.ArrayStoreException:[Ljava.lang.Integer;",         // s28
    };

    static String observeKind(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (ArrayStoreException e) {
            return "ArrayStoreException";
        } catch (IllegalArgumentException e) {
            return "IllegalArgumentException";
        } catch (Throwable t) {
            return "UNEXPECTED:" + t.getClass().getName();
        }
    }

    static String observeWithMessage(int id) {
        try {
            run(id);
            return "no-throw";
        } catch (Throwable t) {
            return t.getClass().getName() + ":" + t.getMessage();
        }
    }

    /**
     * The one evidence funnel. Prints UNCONDITIONALLY and prints the VM's OWN
     * answer, so a row's value reaches the cross-VM diff whether or not this
     * file's table agrees with it.
     */
    static void ck(String row, String fields) {
        System.out.printf("CK RArrayStoreLibrary %-30s %s%n", row, fields);
    }

    static void diverge(String detail) {
        DIVERGENCES.add(detail);
        System.out.println("CK RArrayStoreLibrary FAILED " + detail);
    }

    public static void main(String[] args) {
        // ITERS is a parameter of the measurement, not decoration: read together
        // with a moved= index it is what says whether the compiled tier was
        // reached at all. It goes into the diff for that reason.
        System.out.println("CK RArrayStoreLibrary ITERS=" + ITERS);

        // Pass 1: cold. One execution per site, guaranteed interpreted because
        // nothing has run yet — the only tier where the message is meaningful.
        for (int id = 1; id <= N; id++) {
            String want = EXPECTED_COLD[id];
            if (want == null) continue;
            String got = observeWithMessage(id);
            checks++;
            ck(NAMES[id], "coldmsg=[" + got + "]");
            if (!want.equals(got)) {
                diverge(NAMES[id] + " COLD-MESSAGE: want=[" + want + "] got=[" + got + "]");
            }
        }

        // Pass 2: tier parity on the exception KIND. A native's answer is not
        // automatically tier-invariant — `System.arraycopy` has a JIT intrinsic,
        // and the interpreted and compiled `aastore` are separate emitters — so
        // the iteration at which an answer moved is published on every row and
        // not only on a transition. An absent field is not evidence.
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

            if (!"no-throw".equals(want) && want.equals(hot)) illegalRefused++;
            if ("no-throw".equals(want) && "no-throw".equals(hot)) legalAdmitted++;
        }

        // The balance assertion, and the pair of lines that separate "checked"
        // from "not checked". A door with no check at all satisfies every LEGAL
        // row for free, and the legal rows are the majority; a door that refuses
        // everything satisfies every ILLEGAL row. Both denominators are counted
        // off the table rather than written as literals, so adding a row cannot
        // silently make the ratio wrong instead of making the run red.
        int illegalTotal = 0, legalTotal = 0;
        for (int id = 1; id <= N; id++) {
            if ("no-throw".equals(EXPECTED_KIND[id])) legalTotal++; else illegalTotal++;
        }
        System.out.println("CK RArrayStoreLibrary illegalRefused=" + illegalRefused + "/" + illegalTotal);
        System.out.println("CK RArrayStoreLibrary legalAdmitted=" + legalAdmitted + "/" + legalTotal);
        checks++;
        if (illegalRefused == 0) {
            diverge("DEGENERATE: not one illegal library store was refused - "
                + "the doors have no check, they are not merely wrong");
        }
        checks++;
        if (legalAdmitted == 0) {
            diverge("DEGENERATE: not one legal library store was admitted - "
                + "the doors refuse everything");
        }

        // SEPARATE lines, and in this order: harness_check_count does
        // `sub(/^.*checks=/, ""); print`, so `checks=N fails=0` would publish the
        // "count" `N fails=0`. One value per line.
        System.out.println("CK RArrayStoreLibrary fails=" + DIVERGENCES.size());
        System.out.println("CK RArrayStoreLibrary checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RArrayStoreLibrary (" + checks + " checks)");
    }
}
