// Interpreter round i1 wave 52, lane L1: the `ExceptionCatch` event of the
// C JVMTI table, with the caught object, against HotSpot (the last item of
// docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-table-interpreter-events-through-the-delivery-env-FIXED-20261016.md,
// beside L1W46JvmtiLivePhasePotential).
//
// The shim's `Agent_OnLoad` acquires can_generate_exception_events and sets
// an `ExceptionCatch` callback. The native `rows` enables the event for the
// calling thread, calls `exercise()` through JNI, disables it, and answers
// what the callback saw: for each catch in this class, the catching method,
// the handler's location, the caught object's class and message; the 3000
// catches of the hot loop (which a JIT compiles on CratonVM and HotSpot
// alike) are counted, not listed. The first catch is repeated at the end
// (`again`), so a first-call-only gap shows as a differing row.
//
// Needs the native shim tools/probes/interp/L1/L1W52JvmtiExceptionCatch.c,
// loaded both as an agent and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w52catch.so tools/probes/interp/L1/L1W52JvmtiExceptionCatch.c
//   javac -d /tmp/l1w52catch tools/probes/interp/L1/L1W52JvmtiExceptionCatch.java
//   java     -agentpath:/tmp/libl1w52catch.so -cp /tmp/l1w52catch L1W52JvmtiExceptionCatch /tmp/libl1w52catch.so
//   cratonvm -agentpath:/tmp/libl1w52catch.so -cp /tmp/l1w52catch L1W52JvmtiExceptionCatch /tmp/libl1w52catch.so
//
// Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with a Rust
// port of the C shim (same calls, same order, same output format), three
// runs, the same each time; the handler locations are javac 25's:
//
//   onload: caps err=0 callbacks err=0
//   enable: err=0
//   catch in exercise at 10: Ljava/lang/IllegalStateException; "first"
//   catch in exercise at 27: Ljava/lang/UnsupportedOperationException; "second"
//   catch in exercise at 87: Ljava/lang/IllegalStateException; "again"
//   disable: err=0
//   hot catches: 3000
//   caught=3003
//
// CratonVM before wave 52: `onload: caps err=0 callbacks err=0`, then
// `enable: err=98` (the C table did not deliver `ExceptionCatch`) and no
// catch row, `hot catches: 0`. Since wave 52 (predicted from the code,
// `CRATONVM_JVMTI_C_EXCEPTION_CATCH` on, the default): HotSpot's lines; with
// the switch off, the old lines. The catch rows are the positive control
// (`jvmti::native_env::deliver_exception_catch`); unit test
// `jvmti::native_env::tests::a_c_env_gets_exception_catch_with_the_caught_object`.
public class L1W52JvmtiExceptionCatch {
    static native String rows();

    static int caught;

    static void exercise() {
        try {
            throw new IllegalStateException("first");
        } catch (IllegalStateException e) {
            caught++;
        }
        try {
            thrower("second");
        } catch (RuntimeException e) {
            caught++;
        }
        for (int i = 0; i < 3000; i++) {
            try {
                if (i >= 0) {
                    throw new ArithmeticException("hot");
                }
            } catch (ArithmeticException e) {
                caught++;
            }
        }
        try {
            throw new IllegalStateException("again");
        } catch (IllegalStateException e) {
            caught++;
        }
    }

    static void thrower(String message) {
        throw new UnsupportedOperationException(message);
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W52JvmtiExceptionCatch <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
        System.out.println("caught=" + caught);
    }
}
