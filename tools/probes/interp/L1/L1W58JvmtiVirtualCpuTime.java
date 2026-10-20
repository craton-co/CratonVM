// Interpreter round i2 wave 58, lane L1: the C JVMTI table's
// `GetThreadCpuTime` (137) and `GetCurrentThreadCpuTime` (135) of a virtual
// thread, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 57)" item 1).
//
// The shim's `Agent_OnLoad` adds `can_get_current_thread_cpu_time` and
// `can_get_thread_cpu_time`. Each row prints only the error code:
//
// * `vt null` / `vt self` / `vt current`: inside a virtual thread,
//   `GetThreadCpuTime` of NULL and of itself, `GetCurrentThreadCpuTime`
//   (first);
// * `vt parked`: `main` asks for a virtual thread parked in
//   `LockSupport.park`;
// * `main null` / `main self` / `main current`: the same three on `main`, a
//   platform thread (the control).
//
// Two rounds (the first-call rows again).
//
// Needs the native shim tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w58vcpu.so tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.c
//   javac -d /tmp/l1w58vcpu tools/probes/interp/L1/L1W58JvmtiVirtualCpuTime.java
//   java     -agentpath:/tmp/libl1w58vcpu.so -cp /tmp/l1w58vcpu L1W58JvmtiVirtualCpuTime /tmp/libl1w58vcpu.so
//   cratonvm -agentpath:/tmp/libl1w58vcpu.so -cp /tmp/l1w58vcpu L1W58JvmtiVirtualCpuTime /tmp/libl1w58vcpu.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W58JvmtiVirtualCpuTime (README
// in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   round 1
//   vt null: err=73
//   vt self: err=73
//   vt current: err=73
//   vt parked: err=73
//   main null: err=0
//   main self: err=0
//   main current: err=0
//   round 2
//   (the seven rows again, the same)
//
// (73 is JVMTI_ERROR_UNSUPPORTED_OPERATION: HotSpot has no CPU time for a
// virtual thread, asked by itself or by another thread.)
//
// CratonVM before wave 58 (code read): `err=0` for every `vt` row (the
// carrier's OS thread clock, or the registry's OS thread of the virtual
// thread). Since wave 58: HotSpot's lines
// (`jvmti::native_env::names_virtual_thread`).
import java.util.concurrent.locks.LockSupport;

public class L1W58JvmtiVirtualCpuTime {
    static native String cpu(String label, Thread t);

    static native String currentCpu(String label);

    static volatile boolean go;

    static void round(long n) throws InterruptedException {
        System.out.println("round " + n);
        StringBuilder inside = new StringBuilder();
        Thread vt = Thread.ofVirtual().name("l1w58-vt-" + n).start(() -> {
            inside.append(cpu("vt null", null));
            inside.append(cpu("vt self", Thread.currentThread()));
            inside.append(currentCpu("vt current"));
        });
        vt.join();
        System.out.print(inside);
        go = false;
        Thread parked = Thread.ofVirtual().name("l1w58-parked-" + n).start(() -> {
            while (!go) {
                LockSupport.park();
            }
        });
        long deadline = System.nanoTime() + 10_000_000_000L;
        while (parked.getState() != Thread.State.WAITING) {
            if (System.nanoTime() > deadline) {
                System.out.println("timeout waiting for WAITING (is " + parked.getState() + ")");
                break;
            }
            Thread.sleep(1);
        }
        System.out.print(cpu("vt parked", parked));
        go = true;
        LockSupport.unpark(parked);
        parked.join();
        System.out.print(cpu("main null", null));
        System.out.print(cpu("main self", Thread.currentThread()));
        System.out.print(currentCpu("main current"));
    }

    public static void main(String[] args) throws InterruptedException {
        if (args.length != 1) {
            System.out.println("usage: L1W58JvmtiVirtualCpuTime <path of the shim library>");
            return;
        }
        System.load(args[0]);
        round(1);
        round(2);
    }
}
