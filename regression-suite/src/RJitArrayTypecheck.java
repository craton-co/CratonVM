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
 * (an array receiver is rendered by its component name — a cosmetic defect
 * that is NOT fixed here and made this look like a class-identity split).
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

    public static void main(String[] args) {
        Object strArr = new String[] { "x", "y" };
        Object intArr = new Integer[] { 1, 2 };
        Object intPrimArr = new int[] { 1, 2, 3 };
        Object nestedArr = new String[][] { { "a" } };
        Object str = "plain";
        Object boxed = Integer.valueOf(7);

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

        // checkcast on an array receiver must throw, not silently succeed.
        boolean threw = false;
        try {
            castToStringLen(strArr);
        } catch (ClassCastException expected) {
            threw = true;
        }
        check(threw, "(String) new String[]{...} must throw ClassCastException");

        System.out.println("CK RJitArrayTypecheck sink=" + sink);
        System.out.println("PASS RJitArrayTypecheck (" + checks + " checks)");
    }
}
