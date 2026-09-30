// Interpreter round i1 wave 24, lane L1: a timing probe for the cost a
// debugger's suspension of ONE thread puts on every other thread
// (docs/known-issues/interpreter/i18-L1-proposal-per-thread-interpreter-only-mode-20260925.md,
// "Wave 24 note"). A worker computes in a loop while `main` stops at a
// breakpoint whose request suspends only the event thread (SUSPEND_EVENT_THREAD,
// IntelliJ's "Suspend: Thread"); the debugger holds `main` for three seconds
// and resumes it. The worker's throughput while `main` is suspended, against
// before and after, is the number: HotSpot suspends `main` alone and the
// worker runs on at full speed; CratonVM arms its debugger gate for every
// method of every thread while any thread is suspended
// (`debug::publish_debugger_gates`: `all_methods` from `any_suspension`), so
// the worker runs interpreted, through the per-bytecode suspend point.
//
// Plain run (no debugger; what the probe runner diffs): stdout must equal
// HotSpot 25's:
//
//   plain checksum=OK
//
// Under a debugger (a CratonVM built with --features cratonvm-vm/experimental-debug):
//
//   javac -d out L1W24SuspendedThreadBench.java
//   cratonvm --java-home $JDK --jdwp-port 5771 -cp out L1W24SuspendedThreadBench wait
//   java -cp out L1W24SuspendedThreadBench debug 5771          # HotSpot 25's java
//
// The debuggee prints `batches/s before=B during=D after=A during/before=R`.
// Rows to compare: R on HotSpot (about 1.0: HotSpot 25.0.3 on the i7-8550U
// laptop printed during/before=1.02, 1.00, 1.01 over three runs) against R on
// CratonVM, `--compatible` and default, JIT on; the expected direction of a
// per-thread gate is R rising to about 1 (read from the code, R is well below
// 1 today; not measured here: this lane builds nothing).
public class L1W24SuspendedThreadBench {
    static volatile boolean attached;
    static volatile int phase;
    static volatile boolean stop;
    static final long[] BATCHES = new long[3];
    static final long[] NANOS = new long[3];

    static long work(long seed) {
        long x = seed;
        for (int i = 0; i < 20_000; i++) {
            x = x * 6364136223846793005L + 1442695040888963407L;
            x ^= x >>> 29;
        }
        return x;
    }

    static void pause() {
        // The breakpoint.
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 0) {
            long a = work(1);
            System.out.println("plain checksum=" + (a == work(1) ? "OK" : "BAD"));
            return;
        }
        long until = System.currentTimeMillis() + 300_000;
        while (!attached && System.currentTimeMillis() < until) {
            Thread.sleep(20);
        }
        Thread worker = new Thread(() -> {
            long sink = 0;
            int current = phase;
            long started = System.nanoTime();
            while (!stop) {
                sink += work(sink);
                BATCHES[current]++;
                int now = phase;
                if (now != current) {
                    long t = System.nanoTime();
                    NANOS[current] += t - started;
                    started = t;
                    current = now;
                }
            }
            NANOS[current] += System.nanoTime() - started;
            if (sink == 42) {
                System.out.println("unlikely");
            }
        }, "worker");
        worker.start();
        Thread.sleep(3_000);
        phase = 1;
        pause();
        phase = 2;
        Thread.sleep(3_000);
        stop = true;
        worker.join();
        double[] rate = new double[3];
        for (int p = 0; p < 3; p++) {
            rate[p] = BATCHES[p] / (NANOS[p] / 1e9);
        }
        System.out.printf(java.util.Locale.ROOT,
                "batches/s before=%.1f during=%.1f after=%.1f during/before=%.2f%n",
                rate[0], rate[1], rate[2], rate[1] / rate[0]);
    }

    static final class Debugger {
        static void run(int port) throws Exception {
            com.sun.jdi.connect.AttachingConnector socket = null;
            for (com.sun.jdi.connect.AttachingConnector c :
                    com.sun.jdi.Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            java.util.Map<String, com.sun.jdi.connect.Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            com.sun.jdi.VirtualMachine vm = null;
            long deadline = System.currentTimeMillis() + 120_000;
            while (vm == null) {
                try {
                    vm = socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > deadline) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
            com.sun.jdi.ReferenceType type = null;
            while (type == null) {
                java.util.List<com.sun.jdi.ReferenceType> found =
                        vm.classesByName("L1W24SuspendedThreadBench");
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    type = found.get(0);
                } else {
                    Thread.sleep(50);
                }
            }
            com.sun.jdi.request.BreakpointRequest bp = vm.eventRequestManager()
                    .createBreakpointRequest(type.methodsByName("pause").get(0).location());
            bp.setSuspendPolicy(com.sun.jdi.request.EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ((com.sun.jdi.ClassType) type).setValue(type.fieldByName("attached"), vm.mirrorOf(true));
            while (true) {
                com.sun.jdi.event.EventSet set = vm.eventQueue().remove();
                boolean hit = false;
                for (com.sun.jdi.event.Event e : set) {
                    hit |= e instanceof com.sun.jdi.event.BreakpointEvent;
                    if (e instanceof com.sun.jdi.event.VMDisconnectEvent) {
                        return;
                    }
                }
                if (hit) {
                    Thread.sleep(3_000);
                    set.resume();
                    vm.dispose();
                    System.out.println("held main for 3 s");
                    return;
                }
                set.resume();
            }
        }
    }
}
