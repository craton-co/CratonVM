// Interpreter round i2 wave 61, lane L1: two of the three conformance probes
// of docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-suspension-and-frame-control-FIXED-20261025.md
// (item 5 of its wave-51 triage, from the retired
// `interpreter-L1-proposal-jvmti-c-table-conformance-probes`):
// `SetEventNotificationMode`'s argument and phase errors, and
// `SetBreakpoint` / `ClearBreakpoint`'s errors, against HotSpot.
//
// The shim's `Agent_OnLoad` records the OnLoad-phase rows (an event that
// needs a capability, a thread given in the OnLoad phase, a bad mode and
// event number) and adds `can_generate_breakpoint_events`; `rows()` prints
// the live-phase rows, each `<row>: err=<jvmtiError number>`. A second env
// obtained in the live phase has no capability. The rows run twice (the
// cold-VM first-call rows).
//
// Needs the native shim tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w61errs.so tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.c
//   javac -d /tmp/l1w61errs tools/probes/interp/L1/L1W61JvmtiEventAndBreakpointErrors.java
//   java     -agentpath:/tmp/libl1w61errs.so -cp /tmp/l1w61errs L1W61JvmtiEventAndBreakpointErrors /tmp/libl1w61errs.so
//   cratonvm -agentpath:/tmp/libl1w61errs.so -cp /tmp/l1w61errs L1W61JvmtiEventAndBreakpointErrors /tmp/libl1w61errs.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W61JvmtiEventAndBreakpointErrors
// (README in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload enable SingleStep without capability: err=99
//   onload mode 7: err=0
//   onload event 49: err=0
//   onload enable ThreadStart: err=0
//   onload disable ThreadStart: err=0
//   onload AddCapabilities: err=0
//   onload enable Breakpoint: err=0
//   onload disable Breakpoint: err=0
//   round 1
//   mode 7: err=0
//   event 49: err=0
//   event 89: err=102
//   event 72: err=0
//   event 85: err=0
//   VMInit for a thread: err=103
//   Breakpoint for a class: err=10
//   Breakpoint for an unstarted thread: err=15
//   SingleStep without capability: err=99
//   disable Breakpoint never enabled: err=0
//   enable Breakpoint for this thread: err=0
//   disable Breakpoint for this thread: err=0
//   no capability enable Breakpoint: err=99
//   target locations: 0..7
//   SetBreakpoint NULL method: err=23
//   SetBreakpoint location -1: err=24
//   SetBreakpoint past the end: err=24
//   SetBreakpoint 1000: err=24
//   SetBreakpoint native: err=24
//   SetBreakpoint abstract: err=24
//   SetBreakpoint 0: err=0
//   SetBreakpoint 0 again: err=40
//   SetBreakpoint end: err=0
//   ClearBreakpoint 0: err=0
//   ClearBreakpoint 0 again: err=41
//   ClearBreakpoint end: err=0
//   ClearBreakpoint NULL method: err=23
//   ClearBreakpoint location -1: err=24
//   ClearBreakpoint native: err=24
//   no capability SetBreakpoint: err=99
//   no capability ClearBreakpoint: err=99
//   round 2
//   mode 7: err=0
//   event 49: err=0
//   event 89: err=102
//   event 72: err=0
//   event 85: err=0
//   VMInit for a thread: err=103
//   Breakpoint for a class: err=10
//   Breakpoint for an unstarted thread: err=15
//   SingleStep without capability: err=99
//   disable Breakpoint never enabled: err=0
//   enable Breakpoint for this thread: err=0
//   disable Breakpoint for this thread: err=0
//   no capability enable Breakpoint: err=99
//   target locations: 0..7
//   SetBreakpoint NULL method: err=23
//   SetBreakpoint location -1: err=24
//   SetBreakpoint past the end: err=24
//   SetBreakpoint 1000: err=24
//   SetBreakpoint native: err=24
//   SetBreakpoint abstract: err=24
//   SetBreakpoint 0: err=0
//   SetBreakpoint 0 again: err=40
//   SetBreakpoint end: err=0
//   ClearBreakpoint 0: err=0
//   ClearBreakpoint 0 again: err=41
//   ClearBreakpoint end: err=0
//   ClearBreakpoint NULL method: err=23
//   ClearBreakpoint location -1: err=24
//   ClearBreakpoint native: err=24
//   no capability SetBreakpoint: err=99
//   no capability ClearBreakpoint: err=99
//   target(3)=7
//
// CratonVM before wave 61 (code read): `mode 7` was `err=103` and `event 49`
// `err=102` (both rows, OnLoad and live), `event 72` / `event 85` `err=98`,
// and `SetBreakpoint` / `ClearBreakpoint` of a NULL method decoded it as
// method 0 of class id 0. Since wave 61 (a debug `cratonvm` with
// `experimental-debug`, `--jdk-only` and `--compatible`): HotSpot's lines.
public class L1W61JvmtiEventAndBreakpointErrors {
    static native String onload();

    /** The live-phase rows; `unstarted` is a `Thread` never started. */
    static native String rows(Thread unstarted);

    static native int nativeTarget();

    static int target(int x) {
        int y = x * 2;
        return y + 1;
    }

    abstract static class Shape {
        abstract int sides();
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W61JvmtiEventAndBreakpointErrors <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(onload());
        Thread unstarted = new Thread(() -> { }, "unstarted");
        for (int round = 1; round <= 2; round++) {
            System.out.println("round " + round);
            System.out.print(rows(unstarted));
        }
        System.out.println("target(3)=" + target(3));
    }
}
