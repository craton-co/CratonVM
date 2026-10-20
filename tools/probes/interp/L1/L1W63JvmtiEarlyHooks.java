// The C JVMTI table's OnLoad capabilities `can_generate_early_vmstart` and
// `can_generate_early_class_hook_events` against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 61)", item 6: refused until interpreter round i2
// wave 63). The shim's `Agent_OnLoad` adds both and
// `can_generate_all_class_hook_events`, sets `VMStart`, `VMInit` and
// `ClassFileLoadHook` callbacks and enables the three; the hook notes the
// phase of its first call, how many calls came before `VMStart` and whether
// `java/lang/Object` was offered then; `VMStart` notes its phase.
//
// Needs the native shim tools/probes/interp/L1/L1W63JvmtiEarlyHooks.c,
// loaded both as an agent and with `System.load` (for the native):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63early.so tools/probes/interp/L1/L1W63JvmtiEarlyHooks.c
//   javac -d /tmp/l1w63early tools/probes/interp/L1/L1W63JvmtiEarlyHooks.java
//   java     -agentpath:/tmp/libl1w63early.so -cp /tmp/l1w63early L1W63JvmtiEarlyHooks /tmp/libl1w63early.so
//   cratonvm -agentpath:/tmp/libl1w63early.so -cp /tmp/l1w63early L1W63JvmtiEarlyHooks /tmp/libl1w63early.so
//
// The Rust port of the shim is
// tools/probes/interp/L1/rust-ports/L1W63JvmtiEarlyHooks.
//
// Expected stdout (HotSpot 25.0.3): see the block at the end of this file.
public class L1W63JvmtiEarlyHooks {
    static native String rows();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiEarlyHooks <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
    }
}

// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload: potential early_vmstart=1 early_class_hook=1 add=0
//   vmstart: seen=1 phase=6
//   hook: first phase=2 before vmstart=1 object before vmstart=1
//   vminit: seen=1
