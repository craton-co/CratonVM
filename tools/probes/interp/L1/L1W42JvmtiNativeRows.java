// Interpreter round i1 wave 42, lane L1: the JVMTI stack functions list the
// JNI native methods the calling thread runs, each at location -1, where
// they are on its stack: a native `frames()` that reads its own thread's
// stack is at depth 0, and a native `nested()` whose upcall `up()` calls
// `frames()` is listed between `up` and its caller. Fixes
// docs/internal/fixed-bugs/interpreter-L1-jvmti-stack-functions-omit-the-native-method-that-calls-them-FIXED-20261006.md
// (the wave-41 probe L1W41JvmtiStackFromNative covers the first case with
// GetFrameLocation as well).
//
// Needs the native shim tools/probes/interp/L1/L1W42JvmtiNativeRows.c, a
// plain JNI library that asks `GetEnv` for a JVMTI env (the stack functions
// need no capability):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w42rows.so tools/probes/interp/L1/L1W42JvmtiNativeRows.c
//   javac -d /tmp/l1w42r tools/probes/interp/L1/L1W42JvmtiNativeRows.java
//   java     -cp /tmp/l1w42r L1W42JvmtiNativeRows /tmp/libl1w42rows.so
//   cratonvm -cp /tmp/l1w42r L1W42JvmtiNativeRows /tmp/libl1w42rows.so
//
// Without the argument it prints only the usage line (HotSpot and CratonVM
// alike). Each line is `<case>: count=<GetFrameCount> <frames, innermost
// first>`, a frame being its method name, with `@-1` for location -1.
//
// Expected stdout (HotSpot 25), from the JVMTI specification (a native
// method's frame is a frame, at location -1); NOT RUN on HotSpot with the
// shim (the Windows box that wrote the probe has no C compiler): the
// orchestrator should run the HotSpot line above once and correct this block
// if anything differs.
//
//   direct: count=4 frames@-1 b a main
//   nested: count=6 frames@-1 up nested@-1 c a2 main
//
// CratonVM before wave 42 (`frames_of` listed no native): `direct: count=3 b
// a main` and `nested: count=4 up c a2 main`. Positive control:
// CRATONVM_FRAME_TRACE=1 prints `[JVMTI_NATIVE_ROWS] natives=1 ...` for the
// first line and `natives=2` for the second.
public class L1W42JvmtiNativeRows {
    /** The calling thread's stack, one line (see the header). */
    static native String frames();

    /** Calls `up()` back through JNI and returns its answer. */
    static native String nested();

    static String up() {
        return frames();
    }

    static String b() {
        return frames();
    }

    static String a() {
        return b();
    }

    static String c() {
        return nested();
    }

    static String a2() {
        return c();
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W42JvmtiNativeRows <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        System.out.println("direct: " + a());
        System.out.println("nested: " + a2());
    }
}
