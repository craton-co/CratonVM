import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.io.Serializable;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicReference;

/**
 * Regression: a {@code multianewarray} must produce the SAME runtime array
 * class in the interpreter and in JIT-compiled code.
 *
 * <h2>The defect</h2>
 *
 * The x64 single-pass backend lowered {@code multianewarray} to a helper that
 * was handed only a leaf ELEMENT TYPE code — {@code T_INT}, {@code T_DOUBLE},
 * or "reference". An element type names no class, so the helper allocated both
 * the outer and the inner arrays with {@code ClassId(0)}: the object carried no
 * array class at all, and {@code getClass().getName()} read back
 * {@code [Ljava.lang.Object;} for what the source declared as
 * {@code String[][]}. The interpreter's arm resolved the per-level component
 * classes correctly, so the answer changed the moment a method tiered up.
 *
 * That is invisible until something asks for the type. Two things do:
 *
 * <ul>
 *   <li>a {@code checkcast} back to the declared array type — Commons Math's
 *       {@code DSCompiler.getCompiler} publishes a {@code DSCompiler[][]}
 *       through an {@code AtomicReference} and casts it back on the next call,
 *       which failed 118 of {@code DerivativeStructureTest}'s 124 methods with
 *       the JIT on and 0 of them under {@code --nojit};</li>
 *   <li>{@code ObjectInputStream}'s reflective field restoration, which
 *       rejects an array whose runtime class does not match the declared field
 *       type — the same signature reached through no bytecode
 *       {@code checkcast} at all.</li>
 * </ul>
 *
 * <h2>Why a two-tier fixture</h2>
 *
 * The property under test is tier AGREEMENT, so each shape is evaluated in a
 * loop and its answer recorded cold (first iteration, interpreted), at the
 * exact iteration it changes, and hot (last iteration, compiled). A shape
 * passes only when cold == hot == the HotSpot oracle value. Reading each shape
 * once would measure only the interpreter — the tier that was already right —
 * and go green on a fully broken VM. This is the same design as
 * {@code RArrayStoreTiers}, and for the same reason.
 *
 * <h2>Iteration count</h2>
 *
 * {@code ITERS = 3000}. The C1 invocation threshold is 500
 * ({@code jit/src/tiered.rs}, {@code CompilationPolicy::default}). Crossing it
 * only ENQUEUES the method; the compiled entry is installed some time later, so
 * a fixture that stops near the threshold can finish before compiled code is
 * ever entered. The defect first appeared at iteration 515 in a standalone
 * probe, so 3000 leaves ample margin. If a {@code moved=} number is ever close
 * to {@code ITERS}, raise {@code ITERS} rather than trusting the result.
 *
 * <h2>Run it BOTH ways</h2>
 *
 * Normally and with {@code --nojit}: red without the flag and green with it
 * isolates the divergence to the compiled tier, which is where this one lived.
 */
public final class RJitMultiArrayClass {

    private static final int ITERS = 3000;

    private static final List<String> DIVERGENCES = new ArrayList<>();

    private static void ck(String name, String evidence) {
        System.out.println("CK RJitMultiArrayClass " + name + " " + evidence);
    }

    private static void diverge(String message) {
        DIVERGENCES.add(message);
        System.out.println("FAILED RJitMultiArrayClass " + message);
    }

    // --- allocation sites -------------------------------------------------
    //
    // One method per shape so each gets its own compiled artifact; dimensions
    // come in as arguments so nothing can be constant-folded into a shape the
    // real defect never saw.

    private static Object refLeaf(int a, int b)       { return new String[a][b]; }
    private static Object doubleLeaf(int a, int b)    { return new double[a][b]; }
    private static Object intLeaf(int a, int b)       { return new int[a][b]; }
    private static Object ifaceLeaf(int a, int b)     { return new Runnable[a][b]; }
    private static Object ownLeaf(int a, int b)       { return new RJitMultiArrayClass[a][b]; }
    // 3 brackets in the descriptor but only 2 dimensions allocated: JVMS §4.9.1
    // allows that, and the deepest ALLOCATED level then holds references rather
    // than the leaf type.
    private static Object partialRef(int a, int b)    { return new String[a][b][]; }
    private static Object partialPrim(int a, int b)   { return new int[a][b][]; }

    private static String nameOf(Object o)            { return o.getClass().getName(); }
    private static String componentOf(Object o)       { return o.getClass().getComponentType().getName(); }
    private static String innerOf(Object o)           { return ((Object[]) o)[0].getClass().getName(); }

    // --- the two witness shapes -------------------------------------------

    private static final AtomicReference<String[][]> PUBLISHED = new AtomicReference<>(null);

    /** The `DSCompiler.getCompiler` shape: publish through an AtomicReference, cast back. */
    private static String checkcastRoundTrip(int a, int b) {
        PUBLISHED.set(new String[a][b]);
        Object opaque = PUBLISHED.get();
        try {
            String[][] back = (String[][]) opaque;
            return back.getClass().getName();
        } catch (ClassCastException e) {
            return "CCE";
        }
    }

    private static final class Holder implements Serializable {
        private static final long serialVersionUID = 1L;
        double[][] data;
        Holder(int a, int b) { this.data = new double[a][b]; }
    }

    /** The `NordsieckStepInterpolatorTest.serialization` shape: no bytecode checkcast. */
    private static String serialRoundTrip(int a, int b) {
        try {
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            try (ObjectOutputStream oos = new ObjectOutputStream(bos)) {
                oos.writeObject(new Holder(a, b));
            }
            try (ObjectInputStream ois =
                    new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
                return ((Holder) ois.readObject()).data.getClass().getName();
            }
        } catch (Throwable t) {
            return "threw-" + t.getClass().getName();
        }
    }

    /**
     * A ClassId(0) array has no component type, so an illegal
     * {@code aastore} into it cannot be refused. With the class restored, the
     * JVMS §aastore covariance check applies again — both the legal store and
     * the illegal one are asserted, so a blanket refusal is a failure too.
     */
    private static String storeCheck(int a, int b) {
        Object[][] arr = new String[a][b];
        try {
            arr[0][0] = "legal";
        } catch (Throwable t) {
            return "legal-store-refused-" + t.getClass().getSimpleName();
        }
        Object[] inner = arr[0];
        try {
            inner[1] = Integer.valueOf(3);
            return "illegal-store-allowed";
        } catch (ArrayStoreException e) {
            return "ASE";
        } catch (Throwable t) {
            return "wrong-" + t.getClass().getSimpleName();
        }
    }

    /** A negative dimension must raise NegativeArraySizeException, not yield null. */
    private static String negativeDim(int a, int b) {
        try {
            Object o = refLeaf(a, b);
            return o == null ? "null" : "no-throw";
        } catch (NegativeArraySizeException e) {
            return "NASE";
        } catch (Throwable t) {
            return "wrong-" + t.getClass().getName();
        }
    }

    // --- vector table -----------------------------------------------------

    private static final String[] NAMES = {
        "s00-String[2][3]",
        "s01-String[2][3].component",
        "s02-String[2][3].inner",
        "s03-double[2][3]",
        "s04-double[2][3].component",
        "s05-double[2][3].inner",
        "s06-int[2][3]",
        "s07-Runnable[2][3]",
        "s08-Self[2][3]",
        "s09-String[2][3][]",
        "s10-String[2][3][].inner",
        "s11-int[2][3][]",
        "s12-String[0][0]",
        "s13-String[1][0]",
        "s14-instanceof-String[][]",
        "s15-instanceof-Integer[][]",
        "s16-checkcast-roundtrip",
        "s17-aastore-covariance",
        "s18-negative-outer",
        "s19-negative-inner",
        "s20-serial-roundtrip",
    };

    private static final String[] EXPECTED = {
        "[[Ljava.lang.String;",
        "[Ljava.lang.String;",
        "[Ljava.lang.String;",
        "[[D",
        "[D",
        "[D",
        "[[I",
        "[[Ljava.lang.Runnable;",
        "[[LRJitMultiArrayClass;",
        "[[[Ljava.lang.String;",
        "[[Ljava.lang.String;",
        "[[[I",
        "[[Ljava.lang.String;",
        "[[Ljava.lang.String;",
        "true",
        "false",
        "[[Ljava.lang.String;",
        "ASE",
        "NASE",
        "NASE",
        "[[D",
    };

    private static String observe(int id) {
        switch (id) {
            case 0:  return nameOf(refLeaf(2, 3));
            case 1:  return componentOf(refLeaf(2, 3));
            case 2:  return innerOf(refLeaf(2, 3));
            case 3:  return nameOf(doubleLeaf(2, 3));
            case 4:  return componentOf(doubleLeaf(2, 3));
            case 5:  return innerOf(doubleLeaf(2, 3));
            case 6:  return nameOf(intLeaf(2, 3));
            case 7:  return nameOf(ifaceLeaf(2, 3));
            case 8:  return nameOf(ownLeaf(2, 3));
            case 9:  return nameOf(partialRef(2, 3));
            case 10: return innerOf(partialRef(2, 3));
            case 11: return nameOf(partialPrim(2, 3));
            case 12: return nameOf(refLeaf(0, 0));
            case 13: return nameOf(refLeaf(1, 0));
            case 14: return String.valueOf(refLeaf(2, 3) instanceof String[][]);
            case 15: return String.valueOf(refLeaf(2, 3) instanceof Integer[][]);
            case 16: return checkcastRoundTrip(2, 3);
            case 17: return storeCheck(2, 3);
            case 18: return negativeDim(-1, 3);
            case 19: return negativeDim(3, -1);
            case 20: return serialRoundTrip(2, 3);
            default: throw new AssertionError("no such shape " + id);
        }
    }

    public static void main(String[] args) {
        int checks = 0;

        for (int id = 0; id < NAMES.length; id++) {
            // The serialization shape is two orders of magnitude more expensive
            // than the rest; it still crosses the C1 threshold with room to
            // spare, and its allocation site (`new double[a][b]` in Holder's
            // constructor) is also driven by s03-s05 at the full count.
            int iters = (id == 20) ? 700 : ITERS;

            String cold = null;
            String hot = null;
            String moved = null;
            int movedAt = -1;

            for (int i = 0; i < iters; i++) {
                String a = observe(id);
                if (i == 0) {
                    cold = a;
                } else if (movedAt < 0 && !a.equals(cold)) {
                    movedAt = i;
                    moved = a;
                }
                hot = a;
            }

            String name = NAMES[id];
            String want = EXPECTED[id];

            // Evidence FIRST, verdicts after, so a FAILED line always follows
            // the values it is about. `moved` is published on every row and not
            // only on a transition: the iteration at which an answer moved is
            // precisely the shape a tier-dependent miscompile has.
            ck(name, "cold=[" + cold + "] hot=[" + hot + "] moved=" + movedAt + " iters=" + iters);

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
        }

        // SEPARATE lines, and in this order — see the note in RArrayStoreTiers:
        // harness_check_count does `sub(/^.*checks=/, ""); print`, so
        // `checks=63 fails=0` would publish the "count" `63 fails=0`.
        System.out.println("CK RJitMultiArrayClass fails=" + DIVERGENCES.size());
        System.out.println("CK RJitMultiArrayClass checks=" + checks);
        if (!DIVERGENCES.isEmpty()) {
            throw new AssertionError(DIVERGENCES.size() + " divergence(s)");
        }
        System.out.println("PASS RJitMultiArrayClass (" + checks + " checks)");
    }
}
