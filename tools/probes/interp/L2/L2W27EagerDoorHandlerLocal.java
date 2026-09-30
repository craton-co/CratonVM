// Lane L2 probe (interpreter round i1 wave 27): a `catch` that reads a local
// assigned before its `try`, in a method with no `athrow`, compiled by the
// eager first-call door (`vm/src/runtime/interpreter.rs`, reachable only with
// `CRATONVM_BG_COMPILE=0`). That door refuses a method with an exception table
// only when it also has an `athrow` ("early-rbc6-athrow-with-handler"); it does
// not ask `local_handler_reads_unsafe_local`, and it stages neither the table
// nor precise exceptional frames (`BackendRequest { .. }` without
// `exception_ranges` / `precise_exception_frames`). An exception raised in the
// compiled body then reaches `route_jit_exception_through_method` with no
// precise frame, which seeds the handler frame from the incoming arguments
// only, so `x` below would read 0 in the handler. See
// `docs/internal/fixed-bugs/interpreter-L2-the-eager-first-call-door-compiles-a-handler-method-without-its-exception-table-FIXED-20260929.md`.
//
// Wave 28 (lane L2): JIT round 12 (wave 2, lane tier) already hands a method
// with an exception table from this door to `jit::try_compile`
// (`eager_door_defers_to_try_compile`, default on); the door's own front end
// sees one only under `CRATONVM_JIT_EAGER_ORDINARY_DOOR=0`, and there it now
// stages the table and asks the RBC.6 question as the method-entry door does
// (`BackendRequest::stage_exception_table`,
// `cratonvm_jit::precise_handler_frame_blocking_site`). The last two runs
// below are the ones that reach it; `CRATONVM_DBG_JITC=1` shows either a
// `first-compile` line for `index` / `divide` or the
// `early-rbc6-handler-reads-unsafe-local` seal.
//
// Run on CratonVM:
//   CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> -cp <dir> L2W27EagerDoorHandlerLocal
//   CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> --compatible -cp <dir> L2W27EagerDoorHandlerLocal
//   cratonvm --java-home <jdk25> -cp <dir> L2W27EagerDoorHandlerLocal
//   cratonvm --java-home <jdk25> --nojit -cp <dir> L2W27EagerDoorHandlerLocal
//   CRATONVM_BG_COMPILE=0 CRATONVM_JIT_EAGER_ORDINARY_DOOR=0 cratonvm --java-home <jdk25> -cp <dir> L2W27EagerDoorHandlerLocal
//   CRATONVM_BG_COMPILE=0 CRATONVM_JIT_EAGER_ORDINARY_DOOR=0 cratonvm --java-home <jdk25> --compatible -cp <dir> L2W27EagerDoorHandlerLocal
// stdout must equal HotSpot 25's in all six. HotSpot 25 prints (also -Xint):
//   reflected=14 caught=21
//   index=14 caught=21
//   divide=7 caught=30
//   sum=3350000
// CratonVM before any fix: not run in-lane (constructed from the code). A
// wrong answer shows as `caught=0` on one of the first three lines.
public class L2W27EagerDoorHandlerLocal {
    /** The handler reads `x`, assigned before the `try`: not a parameter. */
    static int index(int[] a, int k) {
        int x = k * 3;
        try {
            return a[k];
        } catch (ArrayIndexOutOfBoundsException e) {
            return x;
        }
    }

    static int divide(int n, int d) {
        int x = n + 23;
        try {
            return n / d;
        } catch (ArithmeticException e) {
            return x;
        }
    }

    /** `index` again, first reached through reflection (see `main`). */
    static int reflectedIndex(int[] a, int k) {
        int x = k * 3;
        try {
            return a[k];
        } catch (ArrayIndexOutOfBoundsException e) {
            return x;
        }
    }

    public static void main(String[] args) throws Exception {
        int[] a = {11, 12, 13, 14};
        // The first call is the eager door's compile under CRATONVM_BG_COMPILE=0
        // when the call reaches `execute` (an invokestatic may take a cached
        // dispatch door first, which compiles through `jit::try_compile` and its
        // RBC.6 check; a reflective call is the candidate route to `execute`).
        java.lang.reflect.Method m = L2W27EagerDoorHandlerLocal.class.getDeclaredMethod(
                "reflectedIndex", int[].class, int.class);
        System.out.println("reflected=" + m.invoke(null, a, 3) + " caught=" + m.invoke(null, a, 7));
        System.out.println("index=" + index(a, 3) + " caught=" + index(a, 7));
        System.out.println("divide=" + divide(21, 3) + " caught=" + divide(7, 0));
        long sum = 0;
        for (int i = 0; i < 100_000; i++) {
            sum += index(a, i & 7) + divide(i & 15, i & 1);
        }
        System.out.println("sum=" + sum);
    }
}
