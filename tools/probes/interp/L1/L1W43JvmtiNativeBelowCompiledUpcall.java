// Interpreter round i1 wave 43, lane L1: the reproduction the page
// docs/internal/fixed-bugs/interpreter-L1-a-native-is-listed-below-a-compiled-method-its-upcall-entered-FIXED-20261008.md
// asked for. A JNI native `nested()` calls `up()` back through JNI, and `up()`
// calls the JNI native `frames()`, which reads its own thread's stack with
// the JVMTI stack functions. The first line is taken before `up` is hot; the
// second after 20,000 more calls, when a JIT has compiled `up` and the JNI
// upcall may enter its compiled body with no interpreter frame. HotSpot lists
// `nested` between `up` and its caller `c` both times.
//
// Needs the native shim tools/probes/interp/L1/L1W43JvmtiNativeBelowCompiledUpcall.c
// (L1W42JvmtiNativeRows.c's shim under this class name):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w43up.so tools/probes/interp/L1/L1W43JvmtiNativeBelowCompiledUpcall.c
//   javac -d /tmp/l1w43up tools/probes/interp/L1/L1W43JvmtiNativeBelowCompiledUpcall.java
//   java     -cp /tmp/l1w43up L1W43JvmtiNativeBelowCompiledUpcall /tmp/libl1w43up.so
//   cratonvm -cp /tmp/l1w43up L1W43JvmtiNativeBelowCompiledUpcall /tmp/libl1w43up.so
//
// Without the argument it prints only the usage line (HotSpot and CratonVM
// alike). Each line is `<case>: count=<GetFrameCount> <frames, innermost
// first>`, a frame being its method name, with `@-1` for location -1.
//
// Expected stdout (HotSpot 25), from the JVMTI specification (a native
// method's frame is a frame, at location -1, where it is on the stack); NOT
// RUN on HotSpot with the shim (the Windows box that wrote the probe has no
// C compiler): the orchestrator should run the HotSpot line above once and
// correct this block if anything differs.
//
//   cold: count=5 frames@-1 up nested@-1 c main
//   warm: count=5 frames@-1 up nested@-1 c main
//
// CratonVM, predicted from reading `jvmti::native_env::splice_native_rows`
// (not measured; this probe is what measures it): `cold` matches. `warm`
// differs when the upcall entered `up`'s compiled body: both native rows are
// recorded at the interpreter depth of `c`'s callee, which has no
// interpreter frame, so both are listed above every row, `up`'s compiled row
// included: `warm: count=5 frames@-1 nested@-1 up c main`. If `warm` matches
// on the host, the upcall did not enter compiled code there (a
// `CRATONVM_DBG_JITC=1` run shows whether `up` compiled) and the page's
// shape needs another entry. Positive control: CRATONVM_FRAME_TRACE=1 prints
// `[JVMTI_NATIVE_ROWS] natives=2 ...` for both lines.
// Since wave 44 (lane L3) a row records the JIT entry-chain length at its
// call and goes below a compiled method its own upcall entered, so `warm`
// matches HotSpot either way; `CRATONVM_DBG_STTRACE=1` prints the rows'
// positions (`STTRACE_DBG_ANCHORS ... positions=[..]`) for a read with
// compiled activations.
public class L1W43JvmtiNativeBelowCompiledUpcall {
    /** The calling thread's stack, one line (see the header). */
    static native String frames();

    /** Calls `up()` back through JNI and returns its answer. */
    static native String nested();

    static String up() {
        return frames();
    }

    static String c() {
        return nested();
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W43JvmtiNativeBelowCompiledUpcall <absolute path of the native shim>");
            return;
        }
        System.load(args[0]);
        System.out.println("cold: " + c());
        String last = "";
        for (int i = 0; i < 20_000; i++) {
            last = c();
        }
        System.out.println("warm: " + last);
    }
}
