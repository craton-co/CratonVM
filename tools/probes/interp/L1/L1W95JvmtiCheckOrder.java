// Interpreter round i4 wave 95, lane L1: the C JVMTI table's phase and
// parameter-check order (wave 94's review findings on
// docs/internal/fixed-bugs/interpreter-i54-L1-proposal-one-jvmti-phase-per-vm-DONE-20261224.md).
//
// * `GetThreadLocalStorage` / `SetThreadLocalStorage` are `phase="start"` in
//   jvmti.xml: they answer in the start phase (a `VMStart` callback). Until
//   wave 95 CratonVM refused them there with JVMTI_ERROR_WRONG_PHASE (112).
// * HotSpot's `jvmtiEnter` wrapper checks the phase first, then each
//   parameter in declaration order: `GetClassMethods(klass, count_ptr,
//   methods_ptr)` answers JVMTI_ERROR_INVALID_CLASS (21) for a NULL class
//   even when the out-parameters are NULL too, `GetMethodDeclaringClass
//   (method, class_ptr)` JVMTI_ERROR_INVALID_METHODID (23) for a NULL method;
//   in the dead phase both answer 112 whatever the pointers. CratonVM checked
//   the out-parameters (JVMTI_ERROR_NULL_POINTER, 100) first.
//
// The agent (tools/probes/interp/L1/L1W95JvmtiCheckOrder.c):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w95order.so tools/probes/interp/L1/L1W95JvmtiCheckOrder.c
//   (Windows: cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" L1W95JvmtiCheckOrder.c /Fe:l1w95order.dll)
//   javac -d /tmp/l1w95order tools/probes/interp/L1/L1W95JvmtiCheckOrder.java
//   java     -agentpath:/tmp/libl1w95order.so -cp /tmp/l1w95order L1W95JvmtiCheckOrder
//   cratonvm [--compatible] -agentpath:/tmp/libl1w95order.so -cp /tmp/l1w95order L1W95JvmtiCheckOrder
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, MSVC-built agent; MEASURED in wave 116)
// start: phase=6 set=0 get=0 same=1 get-null-out=100
// live: methods-null-class=21 methods-null-out=100 declaring-null-method=23 declaring-null-out=100 storage=0 same=1
// main
// unload: phase=8 methods=112 declaring=112 set=112 get=112
// HOTSPOT_EXPECTED_END
//
// The wave-116 HotSpot run (JDK 25.0.3, Windows) printed exactly the derived
// block above: the env is valid in `Agent_OnUnload` and the start phase's
// `get-null-out` is 100.
//
// Before wave 95 CratonVM printed `start: phase=6 set=112 get=112 same=0
// get-null-out=112`, `methods-null-class=100`, `declaring-null-method=100`,
// and `unload: ... methods=100 declaring=100`.
public class L1W95JvmtiCheckOrder {
    public static void main(String[] args) {
        System.out.println("main");
    }
}
