import java.nio.ByteBuffer;

/**
 * Two JNI-boundary families that third-party native libraries depend on and
 * CratonVM answered wrongly. See {@code probes/jni_native_interop_probe.c} for
 * the native half and for which library each family came from.
 *
 * <p>Every line prints CK or FAIL against a hardcoded expectation rather than
 * against "did it throw", because both defects were WRONG ANSWERS in the
 * families that did not crash outright — a probe that only asserted
 * non-throwing would have passed through all of them.
 *
 * <p>Run both arms; they must be byte-identical. {@code -Djava.library.path}
 * points at the directory holding the shared object, which both arms load:
 *
 * <pre>
 * gcc -shared -fPIC -O1 -I$JAVA_HOME/include -I$JAVA_HOME/include/linux \
 *     -o out/libjninativeinterop.so probes/jni_native_interop_probe.c
 * javac -d out probes/JniNativeInteropProbe.java
 * java     -Djava.library.path=out -cp out JniNativeInteropProbe
 * cratonvm -Djava.library.path=out -cp out JniNativeInteropProbe
 * </pre>
 */
public class JniNativeInteropProbe {

    // --- GetDirectBufferAddress ---
    static native int directBufferAddressOk(ByteBuffer buf, long capacity);
    static native int directBufferRead(ByteBuffer buf, int index);
    static native void directBufferWrite(ByteBuffer buf, int index, int value);
    static native long directBufferSum(ByteBuffer buf, int off, int len);
    static native int heapBufferAddressIsNull(ByteBuffer buf);
    static native long heapBufferCapacity(ByteBuffer buf);

    // --- a java.lang.Class arriving as a parameter ---
    static native int classParamSameAsFindClass(Class<?> klaz, String jvmName);
    static native int classParamSameAsGlobalRef(Class<?> klaz, String jvmName);
    static native int classParamMethodId(Class<?> klaz);
    static native int classParamStaticFieldId(Class<?> klaz);
    static native int classParamAssignable(Class<?> sub, Class<?> sup);
    static native int classParamIsInstance(Object value, Class<?> klaz);
    static native String classParamGetName(Class<?> klaz);
    static native Object classParamEcho(Class<?> klaz);

    static int failures;

    static void ck(String label, Object got, Object want) {
        boolean ok = got == null ? want == null : got.equals(want);
        if (!ok) {
            failures++;
        }
        System.out.println((ok ? "CK   " : "FAIL ") + label + " = " + got
                + (ok ? "" : " (expected " + want + ")"));
    }

    public static void main(String[] args) {
        System.loadLibrary("jninativeinterop");

        final int CAP = 256;
        ByteBuffer direct = ByteBuffer.allocateDirect(CAP);
        for (int i = 0; i < CAP; i++) {
            direct.put(i, (byte) (i * 3));
        }

        System.out.println("-- GetDirectBufferAddress --");
        ck("address non-null and capacity agrees", directBufferAddressOk(direct, CAP), 1);
        // Java wrote it, C reads it. This is the call that SIGSEGV'd.
        ck("C reads what Java wrote at [0]", directBufferRead(direct, 0), 0);
        ck("C reads what Java wrote at [7]", directBufferRead(direct, 7), 21);
        ck("C reads what Java wrote at [255]", directBufferRead(direct, 255), (255 * 3) & 0xFF);

        // C writes it, Java reads it: the pointer must ALIAS the buffer, not
        // be a snapshot of it.
        directBufferWrite(direct, 4, 0xAB);
        ck("Java reads what C wrote at [4]", direct.get(4) & 0xFF, 0xAB);

        long expected = 0;
        for (int i = 0; i < 16; i++) {
            expected += direct.get(8 + i) & 0xFF;
        }
        ck("C walks base+offset over 16 bytes", directBufferSum(direct, 8, 16), expected);

        // A slice has folded its own offset into the address field; C must see
        // the slice's base, not the parent's.
        direct.position(64).limit(96);
        ByteBuffer slice = direct.slice();
        direct.clear();
        ck("C reads a slice's [0] as the parent's [64]",
                directBufferRead(slice, 0), direct.get(64) & 0xFF);

        // A heap buffer must answer NULL / -1. On JDK 21+ its `Buffer.address`
        // holds the array base offset (16), so "address != 0" is not the test.
        ck("a heap buffer answers NULL", heapBufferAddressIsNull(ByteBuffer.allocate(16)), 1);
        ck("a wrapped array answers NULL",
                heapBufferAddressIsNull(ByteBuffer.wrap(new byte[16])), 1);
        ck("a heap buffer's capacity is -1", heapBufferCapacity(ByteBuffer.allocate(16)), -1L);
        ck("a direct buffer's capacity is real", heapBufferCapacity(direct), (long) CAP);

        System.out.println("-- Class as a native parameter --");
        ck("IsSameObject(param, FindClass) Boolean",
                classParamSameAsFindClass(Boolean.class, "java/lang/Boolean"), 1);
        ck("IsSameObject(param, FindClass) Integer",
                classParamSameAsFindClass(Integer.class, "java/lang/Integer"), 1);
        ck("IsSameObject(param Boolean, FindClass Integer)",
                classParamSameAsFindClass(Boolean.class, "java/lang/Integer"), 0);
        ck("IsSameObject(param, NewGlobalRef(FindClass))",
                classParamSameAsGlobalRef(Boolean.class, "java/lang/Boolean"), 1);
        ck("GetMethodID(param, toString)", classParamMethodId(Boolean.class), 1);
        ck("GetStaticFieldID(param, TRUE)", classParamStaticFieldId(Boolean.class), 1);
        ck("IsAssignableFrom(param Integer, param Number)",
                classParamAssignable(Integer.class, Number.class), 1);
        ck("IsAssignableFrom(param Integer, param String)",
                classParamAssignable(Integer.class, String.class), 0);
        ck("IsInstanceOf(TRUE, param Boolean)",
                classParamIsInstance(Boolean.TRUE, Boolean.class), 1);
        ck("IsInstanceOf(TRUE, param Integer)",
                classParamIsInstance(Boolean.TRUE, Integer.class), 0);

        // The reverse direction: the same reference is still an ordinary object.
        ck("virtual call on the param", classParamGetName(Boolean.class), "java.lang.Boolean");
        ck("the param round-trips back to Java", classParamEcho(Boolean.class), Boolean.class);

        System.out.println(failures == 0
                ? "PASS JniNativeInteropProbe"
                : "FAIL JniNativeInteropProbe (" + failures + " checks)");
    }
}
