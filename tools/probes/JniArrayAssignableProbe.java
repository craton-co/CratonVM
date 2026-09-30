/**
 * JNI array-alias probe (round 11 wave 13, `r11w13-rt-array-alias-audit-jni-refs-gc`,
 * item 2; written by gc-common w36-c).
 *
 * <p>An array's object header carries its COMPONENT's class id, so a JNI entry
 * point that asks the header answers for the component. Wave 13 fixed
 * {@code IsInstanceOf} and {@code GetObjectClass} for arrays, and wave 15 fixed
 * {@code IsAssignableFrom} between two array classes (covariance), with unit
 * tests that run only when the bare test VM can load the array classes. This
 * probe asks the same questions through real JNI calls, against real JDK
 * classes, so its output can be diffed against HotSpot.
 *
 * <p>Needs the native shim {@code tools/probes/jni/JniArrayAssignableProbe.c}.
 * Build it against the JDK's headers and pass its absolute path as the only
 * argument:
 *
 * <pre>
 *   # Linux
 *   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
 *       -o /tmp/libjniarrayprobe.so tools/probes/jni/JniArrayAssignableProbe.c
 *   javac -d /tmp/jniarr tools/probes/JniArrayAssignableProbe.java
 *   java     -cp /tmp/jniarr JniArrayAssignableProbe /tmp/libjniarrayprobe.so
 *   cratonvm -cp /tmp/jniarr JniArrayAssignableProbe /tmp/libjniarrayprobe.so
 *   # Windows (MSVC): cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32"
 *   #     tools\probes\jni\JniArrayAssignableProbe.c /Fe:jniarrayprobe.dll
 * </pre>
 *
 * <p>Prints one line per case and ends with {@code JNIARRAY_OK cases=N} or
 * {@code JNIARRAY_FAIL failed=K cases=N}. The expected answers are the JLS
 * ones (HotSpot's); every case is also compared with the Java-level
 * {@code Class.isAssignableFrom} / {@code isInstance} / {@code getClass}, whose
 * answer is printed beside it so a disagreement names which door is wrong.
 * Ends on its own; no threads, no timing.
 */
public final class JniArrayAssignableProbe {

    /** JNI {@code IsAssignableFrom(sub, sup)}. */
    static native boolean jniIsAssignableFrom(Class<?> sub, Class<?> sup);

    /** JNI {@code IsInstanceOf(obj, cls)}. */
    static native boolean jniIsInstanceOf(Object obj, Class<?> cls);

    /** JNI {@code GetObjectClass(obj)}. */
    static native Class<?> jniGetObjectClass(Object obj);

    private static int cases;
    private static int failed;

    private static void check(String what, boolean jni, boolean java, boolean expected) {
        cases++;
        boolean ok = jni == expected;
        if (!ok) {
            failed++;
        }
        System.out.println(
                (ok ? "ok   " : "FAIL ") + what + " jni=" + jni + " java=" + java + " expected=" + expected);
    }

    private static void assignable(Class<?> sub, Class<?> sup, boolean expected) {
        check(
                "IsAssignableFrom(" + sub.getName() + ", " + sup.getName() + ")",
                jniIsAssignableFrom(sub, sup),
                sup.isAssignableFrom(sub),
                expected);
    }

    private static void instance(Object obj, Class<?> cls, boolean expected) {
        check(
                "IsInstanceOf(" + obj.getClass().getName() + ", " + cls.getName() + ")",
                jniIsInstanceOf(obj, cls),
                cls.isInstance(obj),
                expected);
    }

    private static void objectClass(Object obj, Class<?> expected) {
        Class<?> jni = jniGetObjectClass(obj);
        check(
                "GetObjectClass(" + obj.getClass().getName() + ") = "
                        + (jni == null ? "null" : jni.getName()),
                jni == expected,
                obj.getClass() == expected,
                true);
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: JniArrayAssignableProbe <absolute path of the native shim>");
            System.out.println("JNIARRAY_FAIL failed=1 cases=0");
            return;
        }
        System.load(args[0]);

        // Array covariance (wave 15's fix): String[] -> Object[] is true.
        assignable(String[].class, Object[].class, true);
        assignable(Object[].class, String[].class, false);
        assignable(Integer[][].class, Number[][].class, true);
        assignable(Number[][].class, Integer[][].class, false);
        assignable(Integer[][].class, Object[].class, true);
        assignable(String[].class, Comparable[].class, true);
        assignable(String[].class, CharSequence[].class, true);
        // Every array is an Object, Cloneable and Serializable.
        assignable(String[].class, Object.class, true);
        assignable(int[].class, Object.class, true);
        assignable(String[].class, Cloneable.class, true);
        assignable(String[].class, java.io.Serializable.class, true);
        // An array is never its component, and primitive arrays are invariant.
        assignable(String[].class, String.class, false);
        assignable(Integer[].class, Number.class, false);
        assignable(int[].class, Object[].class, false);
        assignable(int[].class, long[].class, false);
        // Inherited super-interfaces (wave 13's non-array find).
        assignable(java.util.ArrayList.class, java.util.Collection.class, true);
        assignable(java.util.ArrayList.class, Iterable.class, true);

        // IsInstanceOf on an array object (wave 13's fix).
        instance(new String[0], String.class, false);
        instance(new String[0], Object[].class, true);
        instance(new String[0], Object.class, true);
        instance(new Integer[0], Number.class, false);
        instance(new Integer[0], Number[].class, true);
        instance(new int[0], Object[].class, false);
        instance(new java.util.ArrayList<Object>(), java.util.Collection.class, true);

        // GetObjectClass on an array object (wave 13's fix): the array class,
        // never the component.
        objectClass(new String[0], String[].class);
        objectClass(new Integer[0][0], Integer[][].class);
        objectClass(new int[0], int[].class);
        objectClass(new Object[0], Object[].class);

        System.out.println(
                failed == 0
                        ? "JNIARRAY_OK cases=" + cases
                        : "JNIARRAY_FAIL failed=" + failed + " cases=" + cases);
    }
}
