/*
 * j27 wave 8, lane W8DSP: JNI RegisterNatives on a platform class from
 * application code prints HotSpot's `[warning][jni,resolve] Re-registering of
 * platform native method: ...` line (page
 * docs/feature-designs/native-dispatch-architecture.md).
 *
 * Build (Linux):
 *   javac -d /tmp/j27w8rb tools/probes/jit/j27/J27W8RebindRows.java
 *   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
 *       -o /tmp/libj27w8rb.so tools/probes/jit/j27/J27W8RebindRows.c
 * Run (strip the uptime decoration, which varies):
 *   java --enable-native-access=ALL-UNNAMED -cp /tmp/j27w8rb J27W8RebindRows /tmp/libj27w8rb.so \
 *       | sed -E 's/^\[[0-9.]+s\]//'
 *
 * Expected HotSpot 25 stdout (by reading jni.cpp / method.cpp; take the
 * HotSpot run as the reference if a line differs):
 *   [warning][jni,resolve] Re-registering of platform native method: java.lang.Thread.yield0()V from code in a different classloader
 *   fact boot-native 0
 *   fact boot-native-yield ok
 *   [warning][jni,resolve] Re-registering of platform native method: java.lang.Thread.getName()Ljava/lang/String; from code in a different classloader
 *   fact boot-not-native java.lang.NoSuchMethodError: Method 'java.lang.String java.lang.Thread.getName()' is not declared as native
 *   fact boot-unknown-symbol java.lang.NoSuchMethodError: Method java.lang.Thread.qxzJ9()V not found
 *   fact app-native 0
 *   fact app-native-call 7
 * With -Xlog:jni+resolve=off (or -Xlog:disable): the same without the two
 * [warning] lines.
 */
public class J27W8RebindRows {
    static native Object rebind(Class<?> target, String name, String sig);

    /** Bound only through RegisterNatives (no exported symbol). */
    static native int appNative();

    public static void main(String[] args) {
        System.load(args[0]);
        fact("boot-native", rebind(Thread.class, "yield0", "()V"));
        Thread.yield();
        fact("boot-native-yield", "ok");
        fact("boot-not-native", rebind(Thread.class, "getName", "()Ljava/lang/String;"));
        // Built at run time: a string literal would put the name in the
        // symbol table (a class-file Utf8) and change HotSpot's answer.
        String unknown = new String(new char[] {'q', 'x', 'z', 'J', '9'});
        fact("boot-unknown-symbol", rebind(Thread.class, unknown, "()V"));
        fact("app-native", rebind(J27W8RebindRows.class, "appNative", "()I"));
        fact("app-native-call", appNative());
    }

    static void fact(String key, Object value) {
        String text = value instanceof Throwable t ? t.getClass().getName() + ": " + t.getMessage()
                : String.valueOf(value);
        System.out.println("fact " + key + " " + text);
        System.out.flush();
    }
}
