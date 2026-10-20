// Interpreter round i2 wave 53, lane L6: a JDWP suspension of a thread that
// runs a long native method outside a blocking region (stage 2 of
// docs/internal/fixed-bugs/interpreter-i48-L1-proposal-jdwp-suspension-completes-before-its-reply-DONE-20261224.md,
// `CRATONVM_DEBUG_SUSPEND_NATIVE_ACK`).
//
// The debuggee runs `deflater`, a platform thread that compresses 256 KiB in
// ONE `Deflater.deflate` call (the `deflateBytesBytes` native, about 40 ms at
// level 9 on HotSpot, which blocks nowhere) and bumps a counter after each
// call, so it is almost always inside the native when it is suspended. The
// call is kept short enough to return within the frame commands' own wait
// (`debug::await_suspend_point_frames`): the frames of a thread suspended in
// a native that runs on are a separate gap,
// docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md.
// The debugger suspends it with `ThreadReference.suspend()` and then with
// `VirtualMachine.suspend()`, and each time reads the counter twice around a
// 100 ms pause. HotSpot counts a thread in native suspended at once and
// stops it at its return to Java, before the counter moves: the two reads
// agree and the top frame is the native. CratonVM waited for such a thread
// to leave the native before it answered (up to 250 ms, then a "laggard");
// with the switch on the thread acknowledges the suspension in the native
// and the reply goes out at once, and it parks at the native's return (the
// positive control is the trace below). The transcript is the same either
// way.
//
// Canonical transcript: no ids, no addresses, no timings.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W53JdiSuspendInNative.java
//   cratonvm --java-home $JDK --jdwp-port 5793 -cp out L6W53JdiSuspendInNative wait
//   java -cp out L6W53JdiSuspendInNative debug 5793 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box,
// `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793`):
//
//   == attach
//     thread stable: true
//     thread suspended: true count: 1
//     thread top: deflateBytesBytes native: true
//     vm stable: true
//     vm suspended: true count: 1
//     vm top: deflateBytesBytes native: true
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[SUSPEND_SETTLED]
// waited=1 laggards=0 waited_us=<n> nudges=0 in_native=1` for the two
// suspensions (the switch off: `in_native=0`, and `laggards=1` when the
// native outlasts the 250 ms wait), then `[NATIVE_EXIT_PARK] tid=<n> ...`
// when the native returns.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.zip.Deflater;

public class L6W53JdiSuspendInNative {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once the deflater ran once. */
    static volatile int phase;
    static volatile long counter;
    static volatile boolean stop;

    static void deflate() {
        byte[] in = new byte[256 << 10];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            // A small alphabet: compressible, so level 9 searches hard.
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        byte[] out = new byte[in.length + (1 << 20)];
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        while (!stop) {
            d.reset();
            d.setInput(in);
            d.finish();
            d.deflate(out);
            counter++;
        }
        d.end();
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 300_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread deflater = new Thread(L6W53JdiSuspendInNative::deflate, "deflater");
        deflater.setDaemon(true);
        deflater.start();
        while (counter == 0) {
            Thread.sleep(1);
        }
        if (debugged) {
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        deflater.join(60_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
        }

        static void check(String how, ThreadReference t) throws Exception {
            long first = counter();
            Thread.sleep(100);
            long second = counter();
            System.out.println("  " + how + " stable: " + (first == second));
            System.out.println("  " + how + " suspended: " + t.isSuspended()
                    + " count: " + t.suspendCount());
            Method top = t.frame(0).location().method();
            System.out.println("  " + how + " top: " + top.name() + " native: " + top.isNative());
        }

        static void run(int port) throws Exception {
            AttachingConnector socket = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.name().equals("com.sun.jdi.SocketAttach")) {
                    socket = c;
                }
            }
            Map<String, Connector.Argument> a = socket.defaultArguments();
            a.get("hostname").setValue("localhost");
            a.get("port").setValue(Integer.toString(port));
            a.get("timeout").setValue("60000");
            vm = attach(socket, a);
            System.out.println("== attach");
            type = (ClassType) awaitClass("L6W53JdiSuspendInNative");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 120_000;
            IntegerValue ready;
            do {
                if (System.currentTimeMillis() > until) {
                    fail("the deflater never ran");
                }
                Thread.sleep(20);
                ready = (IntegerValue) type.getValue(type.fieldByName("phase"));
            } while (ready.value() < 2);
            ThreadReference deflater = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("deflater")) {
                    deflater = t;
                }
            }
            if (deflater == null) {
                fail("no deflater thread");
            }
            // 1. ThreadReference.Suspend.
            deflater.suspend();
            check("thread", deflater);
            deflater.resume();
            Thread.sleep(50);
            // 2. VirtualMachine.Suspend.
            vm.suspend();
            check("vm", deflater);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            vm.resume();
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(120_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (next == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : next) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    try {
                        next.resume();
                    } catch (VMDisconnectedException gone) {
                        break;
                    }
                }
            }
            System.out.println("  disconnected");
        }

        static ReferenceType awaitClass(String name) throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                List<ReferenceType> found = vm.classesByName(name);
                if (!found.isEmpty() && found.get(0).isPrepared()) {
                    return found.get(0);
                }
                if (System.currentTimeMillis() > until) {
                    fail("class " + name + " never loaded");
                }
                Thread.sleep(50);
            }
        }

        static VirtualMachine attach(AttachingConnector socket, Map<String, Connector.Argument> a)
                throws Exception {
            long until = System.currentTimeMillis() + 120_000;
            while (true) {
                try {
                    return socket.attach(a);
                } catch (java.io.IOException notYet) {
                    if (System.currentTimeMillis() > until) {
                        throw notYet;
                    }
                    Thread.sleep(250);
                }
            }
        }

        static void fail(String why) {
            System.out.println("FAILED: " + why);
            try {
                vm.dispose();
            } catch (RuntimeException ignored) {
                // Already gone.
            }
            System.exit(2);
        }
    }
}
