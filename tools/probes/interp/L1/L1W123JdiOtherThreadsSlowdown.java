// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 123, lane L1: the measurement i18-L1 (per-thread
// interpreter-only mode) asks for before anything is built -- how much a
// debug session aimed at ONE thread slows the OTHER threads.
// Page: docs/internal/proposals/interpreter/i18-L1-proposal-per-thread-interpreter-only-mode-20260925.md.
//
// The debuggee runs WORKERS hot compute threads (a small static method in a
// loop, so it compiles) and one `stepper` thread that calls `stepTarget` once a
// millisecond. The debugger moves it through timed windows; the debuggee
// counts the workers' iterations per window and prints them as TIME lines
// (ops/ms, and the ratio to the baseline window):
//
//   warmup      no requests (not reported)
//   baseline    no requests
//   bp-else     a breakpoint in a method nobody calls
//   suspended   `stepper` held at a SUSPEND_EVENT_THREAD breakpoint (the
//               wave-24 shape, L1W24SuspendedThreadBench)
//   stepping    a STEP_LINE / STEP_OVER request on `stepper` only, every step
//               event resumed at once (a debugger stepping one thread while
//               the others run: HotSpot deoptimizes the stepping thread only;
//               CratonVM withdraws every compiled body, VM-wide, wave 37)
//   entry-one   a MethodEntryRequest filtered to `stepper` (addThreadFilter,
//               no class filter), SUSPEND_NONE
//   after       every request deleted
//
// What decides i18-L1: the workers' ratio in `stepping` and `entry-one`
// (HotSpot: about 1.0). Read the recompile cost with CRATONVM_DBG_JITC=1
// (`interpreter-only withdrawal:` lines in the debuggee's stderr).
//
// Plain run (no debugger; the windows advance by themselves, short): stdout
// minus TIME lines must equal the block below.
//
// Under a debugger (port 5751; windows of 3 s):
//   javac -g -d out L1W123JdiOtherThreadsSlowdown.java
//   cratonvm --java-home $JDK --jdwp-port 5751 -cp out L1W123JdiOtherThreadsSlowdown wait > debuggee.txt
//   # HotSpot reference debuggee:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5751 \
//       -cp out L1W123JdiOtherThreadsSlowdown wait > debuggee.txt
//   java -cp out L1W123JdiOtherThreadsSlowdown debug 5751 > transcript.txt
//   (also with --nojit on CratonVM: the interpreted floor the ratios fall to)
//
// Debuggee stdout minus TIME lines is the plain block below; the transcript
// (no ids, no timings) is, on any VM (derived from the JDI specification,
// not measured):
//
//   == attach
//   == bp-else armed
//   == suspended stepper=true
//   == stepping steps>0=true other-threads-stepped=false
//   == entry-one entries>0=true other-threads-reported=false
//   == after
//   == end
//     disconnected
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, plain run, default flags and -Xint identical; TIME lines filtered)
// windows=8 last=done
// ended
// HOTSPOT_EXPECTED_END

import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicLongArray;

public class L1W123JdiOtherThreadsSlowdown {
    static final int WORKERS = 2;
    static final String[] PHASES = {
        "start", "warmup", "baseline", "bp-else", "suspended", "stepping", "entry-one", "after", "done",
    };

    /** Set by the debugger (or by main without one): an index into PHASES. */
    static volatile int phase;
    static volatile boolean stop;
    /** Per worker, padded to separate cache lines. */
    static final AtomicLongArray COUNTS = new AtomicLongArray(WORKERS * 16);
    static volatile long sink;

    static long mix(long x) {
        x ^= x << 13;
        x ^= x >>> 7;
        x ^= x << 17;
        return x + 0x9E3779B97F4A7C15L;
    }

    static void work(int id) {
        long x = id + 1;
        long n = 0;
        while (!stop) {
            for (int i = 0; i < 1024; i++) {
                x = mix(x);
            }
            n += 1024;
            COUNTS.lazySet(id * 16, n);
        }
        sink += x;
    }

    static int stepTarget(int i) {
        int a = i * 3;
        int b = a + 1;
        return b;
    }

    /** The breakpoint of the `bp-else` window: never called. */
    static int neverCalled(int i) {
        return i + 1;
    }

    static void stepLoop() {
        int i = 0;
        while (!stop) {
            i = stepTarget(i) & 0xffff;
            try {
                Thread.sleep(1);
            } catch (InterruptedException e) {
                return;
            }
        }
        sink += i;
    }

    static long total() {
        long t = 0;
        for (int w = 0; w < WORKERS; w++) {
            t += COUNTS.get(w * 16);
        }
        return t;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        List<Thread> threads = new ArrayList<>();
        for (int w = 0; w < WORKERS; w++) {
            final int id = w;
            Thread t = new Thread(() -> work(id), "worker-" + w);
            t.setDaemon(true);
            threads.add(t);
        }
        Thread stepper = new Thread(L1W123JdiOtherThreadsSlowdown::stepLoop, "stepper");
        stepper.setDaemon(true);
        threads.add(stepper);
        for (Thread t : threads) {
            t.start();
        }
        long[] ops = new long[PHASES.length];
        long[] nanos = new long[PHASES.length];
        long until = System.currentTimeMillis() + 600_000;
        int seen = phase;
        long lastOps = total();
        long lastT = System.nanoTime();
        long windowStart = System.currentTimeMillis();
        if (!debugged) {
            phase = 1;
        }
        int reached = 0;
        while (System.currentTimeMillis() < until) {
            int p = phase;
            if (p != seen) {
                long nowOps = total();
                long nowT = System.nanoTime();
                ops[seen] = nowOps - lastOps;
                nanos[seen] = nowT - lastT;
                lastOps = nowOps;
                lastT = nowT;
                seen = p;
                reached++;
                windowStart = System.currentTimeMillis();
                if (p == PHASES.length - 1) {
                    break;
                }
            }
            if (!debugged && seen >= 1 && System.currentTimeMillis() - windowStart >= 250) {
                phase = seen + 1;
            }
            Thread.sleep(1);
        }
        stop = true;
        for (Thread t : threads) {
            t.join(10_000);
        }
        System.out.println("windows=" + reached + " last=" + PHASES[seen]);
        double base = rate(ops[2], nanos[2]);
        for (int p = 2; p < PHASES.length - 1; p++) {
            double r = rate(ops[p], nanos[p]);
            System.out.printf(Locale.ROOT, "TIME %-10s ops/ms=%.0f ratio=%.2f%n", PHASES[p], r, base > 0 ? r / base : 0.0);
        }
        System.out.println("ended");
    }

    static double rate(long ops, long nanos) {
        return nanos <= 0 ? 0.0 : ops / (nanos / 1e6);
    }

    static final class Debugger {
        static final long WINDOW_MS = 3_000;
        static VirtualMachine vm;
        static ClassType type;

        static void setPhase(String name) {
            int p = Arrays.asList(PHASES).indexOf(name);
            try {
                type.setValue(type.fieldByName("phase"), vm.mirrorOf(p));
            } catch (Exception e) {
                fail("setPhase " + name + ": " + e);
            }
        }

        static ThreadReference thread(String name) {
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals(name)) {
                    return t;
                }
            }
            fail("no thread " + name);
            return null;
        }

        static Location firstLine(String method) {
            try {
                return type.methodsByName(method).get(0).allLineLocations().get(0);
            } catch (AbsentInformationException e) {
                fail("compile with -g");
                return null;
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
            type = (ClassType) awaitClass("L1W123JdiOtherThreadsSlowdown");
            EventRequestManager erm = vm.eventRequestManager();
            ThreadReference stepper = thread("stepper");

            setPhase("warmup");
            Thread.sleep(WINDOW_MS);
            setPhase("baseline");
            Thread.sleep(WINDOW_MS);

            BreakpointRequest elsewhere = erm.createBreakpointRequest(firstLine("neverCalled"));
            elsewhere.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            elsewhere.enable();
            setPhase("bp-else");
            System.out.println("== bp-else armed");
            Thread.sleep(WINDOW_MS);
            erm.deleteEventRequest(elsewhere);

            // Hold `stepper` at a breakpoint for a window.
            BreakpointRequest hold = erm.createBreakpointRequest(firstLine("stepTarget"));
            hold.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            hold.enable();
            EventSet held = await(BreakpointEvent.class, 30_000);
            erm.deleteEventRequest(hold);
            setPhase("suspended");
            System.out.println("== suspended stepper=" + (stepper.isSuspended()));
            Thread.sleep(WINDOW_MS);

            // Step that thread, and only it, for a window.
            StepRequest step = erm.createStepRequest(stepper, StepRequest.STEP_LINE, StepRequest.STEP_OVER);
            step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            step.enable();
            setPhase("stepping");
            held.resume();
            int steps = 0;
            boolean otherStepped = false;
            long end = System.currentTimeMillis() + WINDOW_MS;
            while (System.currentTimeMillis() < end) {
                EventSet set = vm.eventQueue().remove(50);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof StepEvent) {
                        steps++;
                        otherStepped |= !((StepEvent) e).thread().name().equals("stepper");
                    }
                }
                set.resume();
            }
            erm.deleteEventRequest(step);
            drain();
            System.out.println("== stepping steps>0=" + (steps > 0) + " other-threads-stepped=" + otherStepped);

            // Method entries of that thread only, for a window.
            MethodEntryRequest entry = erm.createMethodEntryRequest();
            entry.addThreadFilter(stepper);
            entry.setSuspendPolicy(EventRequest.SUSPEND_NONE);
            entry.enable();
            setPhase("entry-one");
            int entries = 0;
            boolean otherReported = false;
            end = System.currentTimeMillis() + WINDOW_MS;
            while (System.currentTimeMillis() < end) {
                EventSet set = vm.eventQueue().remove(50);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof MethodEntryEvent) {
                        entries++;
                        otherReported |= !((MethodEntryEvent) e).thread().name().equals("stepper");
                    }
                }
                set.resume();
            }
            erm.deleteEventRequest(entry);
            drain();
            System.out.println("== entry-one entries>0=" + (entries > 0) + " other-threads-reported=" + otherReported);

            setPhase("after");
            System.out.println("== after");
            Thread.sleep(WINDOW_MS);
            setPhase("done");
            System.out.println("== end");
            boolean ended = false;
            while (!ended) {
                EventSet next;
                try {
                    next = vm.eventQueue().remove(60_000);
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
                    next.resume();
                }
            }
            System.out.println("  disconnected");
        }

        /** Resume whatever the deleted requests left queued. */
        static void drain() throws InterruptedException {
            EventSet set;
            while ((set = vm.eventQueue().remove(200)) != null) {
                set.resume();
            }
        }

        static EventSet await(Class<? extends Event> kind, long ms) throws InterruptedException {
            long until = System.currentTimeMillis() + ms;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(100);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (kind.isInstance(e)) {
                        return set;
                    }
                }
                set.resume();
            }
            fail("no " + kind.getSimpleName());
            return null;
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
