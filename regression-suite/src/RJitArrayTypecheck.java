/**
 * Regression: BUG-JIT-ARRAY-INSTANCEOF-20260726. The JIT's typecheck helper
 * consulted the array-descriptor rule first but only honoured a POSITIVE
 * answer; a negative one fell through to a class-hierarchy comparison against
 * `obj_class_id`, which for a reference array is the header's *component*
 * class id. So `String[] instanceof String` compared `java/lang/String`
 * against `java/lang/String` and answered true — as did
 * `Integer[] instanceof Integer` and, via the subclass walk,
 * `String[] instanceof CharSequence`.
 *
 * The interpreter never had this: its Checkcast/InstanceOf handlers dispatch
 * on array-ness and never reach a hierarchy comparison. So every assertion
 * here passes interpreted and passed on HotSpot while failing under a warmed
 * JIT — the checks must therefore run in their own small methods, called hot
 * enough to compile.
 *
 * Found via H2's `ObjectDataType.getTypeId`, a 15-arm `instanceof` ladder over
 * `Object`: once compiled it classified a `String[]` as TYPE_STRING, and the
 * generic bridge's `checkcast java/lang/String` then blew up with
 * `ClassCastException: java.lang.String cannot be cast to java.lang.String`
 * — an array receiver rendered by its COMPONENT name, which made a plain
 * type error look like a class-identity split and cost a session of
 * misdiagnosis. That second defect is fixed too (2026-07-26) and is asserted
 * below on both the interpreted and the compiled `checkcast` path: the
 * receiver must render as its own descriptor, `[Ljava.lang.String;`, exactly
 * as HotSpot does.
 */
public class RJitArrayTypecheck {
    static int checks = 0;
    static void check(boolean c, String m) { checks++; if (!c) throw new AssertionError(m); }

    // Each in its own method: the bug lives in compiled code, so the call has
    // to be one the JIT actually compiles.
    static boolean isString(Object o)      { return o instanceof String; }
    static boolean isInteger(Object o)     { return o instanceof Integer; }
    static boolean isCharSequence(Object o){ return o instanceof CharSequence; }
    static boolean isObject(Object o)      { return o instanceof Object; }
    static boolean isStringArray(Object o) { return o instanceof String[]; }
    static boolean isObjectArray(Object o) { return o instanceof Object[]; }
    static boolean isCloneable(Object o)   { return o instanceof Cloneable; }
    static boolean isSerializable(Object o){ return o instanceof java.io.Serializable; }
    static int     castToStringLen(Object o) { return ((String) o).length(); }

    /**
     * The ClassCastException message a `(String) o` cast produces, or "" if
     * the cast succeeded. A null message is reported as `<fastthrow>`: once
     * HotSpot's C2 has seen enough of these it throws a preallocated,
     * message-less, stack-trace-less exception (`OmitStackTraceInFastThrow`),
     * so any assertion on a message from an already-hot site has to tolerate
     * that. The strict assertions below therefore run against COLD sites.
     */
    static String cceText(Object o) {
        try {
            castToStringLen(o);
            return "";
        } catch (ClassCastException e) {
            String m = e.getMessage();
            return m == null ? "<fastthrow>" : m;
        }
    }

    /** Strict on a real message; tolerant of HotSpot's fast-throw elision. */
    static void checkCceNames(String msg, String expected, String what) {
        check(!msg.isEmpty(), what + ": cast must throw ClassCastException");
        check(msg.equals("<fastthrow>") || msg.contains(expected),
                what + ": receiver must render as " + expected + ", got: " + msg);
    }

    public static void main(String[] args) {
        Object strArr = new String[] { "x", "y" };
        Object intArr = new Integer[] { 1, 2 };
        Object intPrimArr = new int[] { 1, 2, 3 };
        Object nestedArr = new String[][] { { "a" } };
        Object str = "plain";
        Object boxed = Integer.valueOf(7);

        // Captured BEFORE the warm-up loop: these checkcasts run interpreted
        // (cold) on both VMs, so the messages are real on both.
        String coldStrArr = cceText(strArr);
        String coldNested = cceText(nestedArr);
        String coldPrim = cceText(intPrimArr);
        String coldBoxed = cceText(boxed);

        int bad = 0;
        long sink = 0;
        for (int i = 0; i < 400_000; i++) {
            // The bug: an array is NOT an instance of its component type.
            if (isString(strArr)) bad++;
            if (isInteger(intArr)) bad++;
            if (isCharSequence(strArr)) bad++;
            if (isString(nestedArr)) bad++;
            if (isString(intPrimArr)) bad++;

            // …but every array IS an Object, Cloneable and Serializable, and
            // arrays still satisfy their own and their supertypes' array types.
            if (!isObject(strArr)) bad++;
            if (!isCloneable(strArr)) bad++;
            if (!isSerializable(intPrimArr)) bad++;
            if (!isStringArray(strArr)) bad++;
            if (!isObjectArray(strArr)) bad++;
            if (isStringArray(intArr)) bad++;

            // Non-array receivers must be unaffected.
            if (!isString(str)) bad++;
            if (!isCharSequence(str)) bad++;
            if (!isInteger(boxed)) bad++;
            if (isString(boxed)) bad++;

            sink += castToStringLen(str);
        }
        check(bad == 0, "JIT array-typecheck mismatches: " + bad);
        check(sink == 400_000L * 5, "sink " + sink);

        // checkcast on an array receiver must throw, not silently succeed —
        // and must name the receiver by ITS OWN type, not its component's.
        // Interpreted (cold) sites first: strict on both VMs.
        checkCceNames(coldStrArr, "[Ljava.lang.String;", "interpreted String[]");
        checkCceNames(coldNested, "[[Ljava.lang.String;", "interpreted String[][]");
        checkCceNames(coldPrim, "[I", "interpreted int[]");
        checkCceNames(coldBoxed, "java.lang.Integer", "interpreted Integer");
        // Then the same four through the now-hot (compiled on CratonVM)
        // `castToStringLen`.
        checkCceNames(cceText(strArr), "[Ljava.lang.String;", "compiled String[]");
        checkCceNames(cceText(nestedArr), "[[Ljava.lang.String;", "compiled String[][]");
        checkCceNames(cceText(intPrimArr), "[I", "compiled int[]");
        checkCceNames(cceText(boxed), "java.lang.Integer", "compiled Integer");

        System.out.println("CK RJitArrayTypecheck sink=" + sink);
        System.out.println("PASS RJitArrayTypecheck (" + checks + " checks)");
    }
}
