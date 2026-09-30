// Interpreter round i1 wave 41, lane L1: the JVMTI stack functions called
// from inside a JNI native method, on the calling thread. JVMTI's frames
// include native method frames (a native frame's location is -1): a native
// `frames()` called from `b()` from `a()` from `main` is at depth 0 of its
// own thread, above `b`, `a` and `main`. Filed as (fixed in wave 42)
// docs/internal/fixed-bugs/interpreter-L1-jvmti-stack-functions-omit-the-native-method-that-calls-them-FIXED-20261006.md.
//
// Needs the native shim tools/probes/interp/L1/L1W41JvmtiStackFromNative.c, a
// plain JNI library that asks `GetEnv` for a JVMTI env (the three stack
// functions need no capability):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w41jvmtistack.so tools/probes/interp/L1/L1W41JvmtiStackFromNative.c
//   javac -d /tmp/l1w41s tools/probes/interp/L1/L1W41JvmtiStackFromNative.java
//   java     -cp /tmp/l1w41s L1W41JvmtiStackFromNative /tmp/libl1w41jvmtistack.so
//   cratonvm -cp /tmp/l1w41s L1W41JvmtiStackFromNative /tmp/libl1w41jvmtistack.so
//
// Without the argument it prints only the usage line (HotSpot and CratonVM
// alike).
//
// Expected stdout (HotSpot 25), from the JVMTI specification (a native
// method's frame is a frame, at location -1). NOT RUN on HotSpot with the
// shim: the Windows box that wrote the probe has no C compiler; the
// orchestrator should run the HotSpot line above once and correct this block
// if anything differs.
//
//   GetFrameCount: 4
//   GetFrameLocation 0: frames native=true location=-1
//   GetFrameLocation 1: b native=false location>=0=true
//   GetStackTrace 0..10: frames b a main
//   GetStackTrace -2..: a main
//
// CratonVM, from reading `jvmti::native_env::frames_of` (the interpreter
// frames and compiled activations through
// `stackwalker::capture_full_trace_without_store`, which lists no native
// method): `GetFrameCount: 3`, depth 0 is `b` at its invoke, and the traces
// start at `b` (`b a main`, `a main`). Wave 42 (lane L1) lists the native:
// CratonVM should now print the HotSpot block above.
public class L1W41JvmtiStackFromNative {
    /** One line per stack function, as described in the header. */
    static native String frames();

    static String b() {
        return frames();
    }

    static String a() {
        return b();
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W41JvmtiStackFromNative <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        System.out.print(a());
    }
}
