// Interpreter round i5 wave 107, lane L1: a JNI-attached thread that detaches
// with an exception pending hands it to its uncaught-exception handler
// (docs/internal/fixed-bugs/interpreter-i107-L1-jni-detach-drops-the-pending-exception-FIXED-20261016.md).
//
// HotSpot's `jni_DetachCurrentThread` -> `JavaThread::exit(false,
// jni_detach)` takes the thread's pending exception and calls
// `Thread.dispatchUncaughtException` with it before `Thread.exit()`, so the
// default handler (here a `setDefaultUncaughtExceptionHandler` that prints
// one line) sees it; without a handler the JDK prints `Exception in thread
// "l1w107-pending" ...` on stderr. CratonVM's detach dropped it silently.
// Fixed in wave 107 (vm/src/native/jni.rs `end_foreign_thread_java_side`,
// vm/src/vm/vm_exec.rs `exit_attached_thread_java_side`); not built or run.
//
// Needs the shim tools/probes/interp/L1/L1W107JniDetachPendingException.c;
// tools/jvmti/run-jvmti-shims.sh runs it (a default scenario since wave 107).
// By hand:
//   gcc -shared -fPIC -pthread -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w107pending.so tools/probes/interp/L1/L1W107JniDetachPendingException.c
//   javac -d /tmp/l1w107pending tools/probes/interp/L1/L1W107JniDetachPendingException.java
//   java     -cp /tmp/l1w107pending L1W107JniDetachPendingException /tmp/libl1w107pending.so
//   cratonvm -cp /tmp/l1w107pending L1W107JniDetachPendingException /tmp/libl1w107pending.so
//
// HOTSPOT_EXPECTED_BEGIN
// callback on l1w107-pending
// uncaught in l1w107-pending: java.lang.IllegalStateException: left pending
// attached thread joined
// HOTSPOT_EXPECTED_END
//
// Derived from javaThread.cpp (JDK 25 `JavaThread::exit`), not measured.
// Before the fix CratonVM printed no `uncaught` line.
public class L1W107JniDetachPendingException {
    static native void say(String line);

    static native boolean run();

    static void fromNative() {
        say("callback on " + Thread.currentThread().getName());
        throw new IllegalStateException("left pending");
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W107JniDetachPendingException <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Thread.setDefaultUncaughtExceptionHandler(
                (t, e) -> say("uncaught in " + t.getName() + ": " + e));
        if (!run()) {
            say("run failed");
            return;
        }
        say("attached thread joined");
    }
}
