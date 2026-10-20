// Interpreter round i2 wave 57, lane L6: the frames of a thread suspended
// while it runs a LONG native method outside a blocking region, called from
// a method the JIT compiled
// (docs/internal/fixed-bugs/interpreter-L6-a-thread-suspended-in-a-running-native-lists-no-frames-FIXED-20261007.md).
//
// The debuggee's `deflater` thread first calls `squeeze` 20 000 times on a
// small input, so that `squeeze` (which calls `Deflater.deflate`) runs
// compiled, then calls it in a loop on 16 MiB at level 9: one
// `deflateBytesBytes` call that blocks nowhere and runs for seconds. The
// debugger suspends it in the middle of such a call, with
// `ThreadReference.suspend()` and then with `VirtualMachine.suspend()`, and
// lists its frames at once. HotSpot counts a thread in native suspended at
// once and lists its whole stack while the native still runs: the native on
// top (location -1), then its Java callers, compiled or not. CratonVM's
// native frames window (`CRATONVM_DEBUG_NATIVE_FRAMES_WINDOW`) opened only
// for a thread with no compiled activation (wave 56) and was refused after
// any pause, even one that moved nothing (`[NATIVE_FRAMES_WINDOW] ...
// collected=true`); since wave 57 the thread records its compiled view as it
// opens the window, and only a collection that moved objects makes the
// reader refuse.
//
// Canonical transcript: method names only (no ids, no lines, no timings);
// the JDK frames between the native and `squeeze` are counted, not named,
// and the listing is printed down to the probe's `run` loop.
//
// Plain run (no debugger): stdout must equal HotSpot 25's:
//
//   ended
//
// Under a debugger:
//
//   javac -g -d out L6W57JdiSuspendInNativeUnderCompiledCaller.java
//   cratonvm --java-home $JDK --jdwp-port 5794 -cp out L6W57JdiSuspendInNativeUnderCompiledCaller wait
//   java -cp out L6W57JdiSuspendInNativeUnderCompiledCaller debug 5794 > transcript.txt
//
// HotSpot 25.0.3 as the debuggee (default flags, the Windows box,
// `java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5794`):
//
//   == attach
//     thread stable: true
//     thread top: deflateBytesBytes native: true
//     thread frames: deflateBytesBytes < 2 JDK < squeeze < loop
//     thread top locals: absent
//     vm stable: true
//     vm top: deflateBytesBytes native: true
//     vm frames: deflateBytesBytes < 2 JDK < squeeze < loop
//   == end
//     disconnected
//
// Positive control: `CRATONVM_FRAME_TRACE=1` prints `[NATIVE_FRAMES_WINDOW]
// tid=<n> published=true depth=<n> blocked=false collected=false` when the
// debugger read the running native's frames through the window.
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import java.util.*;
import java.util.zip.Deflater;

public class L6W57JdiSuspendInNativeUnderCompiledCaller {
    /** Set by the debugger: 1 = start, 3 = done. Set to 2 by the debuggee once the deflater is in a long call. */
    static volatile int phase;
    static volatile long counter;
    static volatile boolean inCall;
    static volatile boolean stop;

    static int squeeze(Deflater d, byte[] in, byte[] out) {
        d.reset();
        d.setInput(in);
        d.finish();
        return d.deflate(out);
    }

    static byte[] input(int size) {
        byte[] in = new byte[size];
        long seed = 12345;
        for (int i = 0; i < in.length; i++) {
            seed = seed * 6364136223846793005L + 1442695040888963407L;
            // A small alphabet: compressible, so level 9 searches hard.
            in[i] = (byte) ('a' + (int) ((seed >>> 33) & 7));
        }
        return in;
    }

    static void loop() {
        Deflater d = new Deflater(Deflater.BEST_COMPRESSION);
        byte[] small = input(64);
        byte[] smallOut = new byte[256];
        long sum = 0;
        for (int i = 0; i < 20_000; i++) {
            sum += squeeze(d, small, smallOut);
        }
        byte[] in = input(16 << 20);
        byte[] out = new byte[in.length + (1 << 20)];
        while (!stop) {
            inCall = true;
            sum += squeeze(d, in, out);
            inCall = false;
            counter++;
        }
        d.end();
        if (sum == 42) {
            System.out.println("unlikely");
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        long until = System.currentTimeMillis() + 600_000;
        while (debugged && phase == 0 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        Thread deflater = new Thread(L6W57JdiSuspendInNativeUnderCompiledCaller::loop, "deflater");
        deflater.setDaemon(true);
        deflater.start();
        while (!debugged && counter == 0) {
            Thread.sleep(1);
        }
        if (debugged) {
            while (!inCall) {
                Thread.sleep(1);
            }
            phase = 2;
        }
        while (debugged && phase != 3 && System.currentTimeMillis() < until) {
            Thread.sleep(10);
        }
        stop = true;
        deflater.join(600_000);
        System.out.println("ended");
    }

    static final class Debugger {
        static VirtualMachine vm;
        static ClassType type;

        static long counter() {
            return ((LongValue) type.getValue(type.fieldByName("counter"))).value();
        }

        static String frames(ThreadReference t) throws Exception {
            List<StackFrame> frames = t.frames();
            if (frames.isEmpty()) {
                return "none";
            }
            StringBuilder s = new StringBuilder(frames.get(0).location().method().name());
            int jdk = 0;
            for (int i = 1; i < frames.size(); i++) {
                Location at = frames.get(i).location();
                String owner = at.declaringType().name();
                if (owner.startsWith("java.")) {
                    jdk++;
                    continue;
                }
                if (jdk > 0) {
                    s.append(" < ").append(jdk).append(" JDK");
                    jdk = 0;
                }
                s.append(" < ").append(at.method().name());
                // Down to the probe's `loop`: the lambda and `Thread`
                // frames below it are not this probe's question.
                if (at.method().name().equals("loop")) {
                    break;
                }
            }
            return s.toString();
        }

        static void check(String how, ThreadReference t, boolean locals) throws Exception {
            long first = counter();
            Thread.sleep(100);
            long second = counter();
            System.out.println("  " + how + " stable: " + (first == second));
            List<StackFrame> frames = t.frames();
            if (frames.isEmpty()) {
                System.out.println("  " + how + " top: none");
            } else {
                Method top = frames.get(0).location().method();
                System.out.println("  " + how + " top: " + top.name() + " native: " + top.isNative());
            }
            System.out.println("  " + how + " frames: " + frames(t));
            if (locals && !frames.isEmpty()) {
                String answer;
                try {
                    answer = frames.get(0).visibleVariables().isEmpty() ? "none" : "some";
                } catch (AbsentInformationException absent) {
                    answer = "absent";
                }
                System.out.println("  " + how + " top locals: " + answer);
            }
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
            type = (ClassType) awaitClass("L6W57JdiSuspendInNativeUnderCompiledCaller");
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(1));
            long until = System.currentTimeMillis() + 300_000;
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
            // In the native by now: it entered the call before `phase` was 2.
            Thread.sleep(200);
            // 1. ThreadReference.Suspend.
            deflater.suspend();
            check("thread", deflater, true);
            deflater.resume();
            // 2. VirtualMachine.Suspend, in the next call.
            long before = counter();
            until = System.currentTimeMillis() + 300_000;
            while (counter() == before && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            Thread.sleep(200);
            vm.suspend();
            check("vm", deflater, false);
            type.setValue(type.fieldByName("phase"), vm.mirrorOf(3));
            vm.resume();
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(300_000);
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
