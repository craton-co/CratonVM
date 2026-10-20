// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L1: a breakpoint in a JDK method the
// JIT does NOT expand at a call site (`java.util.TreeMap.put`, outside
// vm/src/debug/jit_expansion_set.rs), set before its hot caller compiles.
// Step 1 of
// docs/internal/fixed-bugs/interpreter-i40-L1-proposal-call-site-intrinsics-that-yield-to-a-breakpoint-DONE-20261224.md:
// with `CRATONVM_JDWP_JDK_BREAKPOINT_EXPANSION_SET=1` such a breakpoint no
// longer keeps the whole VM interpreted (wave 40's hold for every JDK
// class); `mid` compiles, and every hit must still arrive (the scoped
// withdrawal and the compile doors' per-method refusal serve it, as for an
// application class). The sister of L1W40JdiBreakpointInJitIntrinsicCallee
// (`String.charAt`, in the set: must keep its 200 hits with the switch on).
//
// `outer()` calls `mid(i)` 200,000 times; `mid` puts into a TreeMap once
// per 1000 calls. The breakpoint is at TreeMap.put's first location,
// filtered to main, SUSPEND_EVENT_THREAD.
//
// Plain run (no debugger), stdout as HotSpot 25: `sum=700000`.
//
// Under a debugger (debug build), both modes, JIT and --nojit, the switch
// unset and =1:
//
//   javac -g -d out L1W117JdiBreakpointInUnexpandedJdkMethod.java
//   cratonvm --java-home $JDK --jdwp-port 5792 -cp out L1W117JdiBreakpointInUnexpandedJdkMethod wait
//   java -cp out L1W117JdiBreakpointInUnexpandedJdkMethod debug 5792 > transcript.txt
//   (HotSpot: java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5792 -cp out L1W117JdiBreakpointInUnexpandedJdkMethod wait)
//
// Positive control with the switch on: CRATONVM_DBG_JITC=1 prints a
// `breakpoint withdrawal: class=java/util/TreeMap ... scoped=true` line and
// no `interpreter-only withdrawal: source=jdk-breakpoint` line; unset, the
// latter (wave 40's hold).
//
// HotSpot 25.0.3 as the debuggee (run in wave 117, Windows):
//
//   == attach
//   == breakpoint in TreeMap.put before its caller compiles
//     hits from mid: 200
//     last hit in: java.util.TreeMap.put index=0
//     its caller: mid
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L1W117JdiBreakpointInUnexpandedJdkMethod {
    /** 0 until the debugger has set its breakpoint. */
    static volatile int go;
    static long sum;

    static final java.util.TreeMap<Integer, Integer> MAP = new java.util.TreeMap<>();

    static long mid(int x) {
        if (x % 1000 == 999) {
            MAP.put(x, x);
        }
        return x & 7;
    }

    static long outer() {
        long s = 0;
        for (int i = 0; i < 200_000; i++) {
            s += mid(i);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        if (args.length == 1 && args[0].equals("wait")) {
            long until = System.currentTimeMillis() + 300_000;
            while (go == 0 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        }
        sum = outer();
        System.out.println("sum=" + sum);
    }

    static final class Debugger {
        static VirtualMachine vm;

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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ReferenceType type = awaitClass("L1W117JdiBreakpointInUnexpandedJdkMethod");
            ThreadReference main = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("main")) {
                    main = t;
                }
            }
            if (main == null) {
                fail("no main thread");
            }
            ReferenceType treeMap = awaitClass("java.util.TreeMap");
            Method put = null;
            for (Method m : treeMap.methodsByName("put")) {
                if (m.signature().equals("(Ljava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;")) {
                    put = m;
                }
            }
            if (put == null) {
                fail("no TreeMap.put(Object, Object)");
            }
            Location at = put.location();

            System.out.println("== breakpoint in TreeMap.put before its caller compiles");
            BreakpointRequest bp = erm.createBreakpointRequest(at);
            bp.addThreadFilter(main);
            bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            bp.enable();
            ((ClassType) type).setValue(type.fieldByName("go"), vm.mirrorOf(1));
            int hits = 0;
            String lastIn = "-";
            String lastCaller = "-";
            long until = System.currentTimeMillis() + 120_000;
            boolean ended = false;
            while (!ended && System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(2_000);
                if (set == null) {
                    // No hit for two seconds: the loop is over (or every later
                    // hit was missed).
                    if (hits > 0) {
                        break;
                    }
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent b) {
                        String caller = b.thread().frame(1).location().method().name();
                        if (caller.equals("mid")) {
                            hits++;
                            Location l = b.location();
                            lastIn = l.declaringType().name() + "." + l.method().name()
                                    + " index=" + l.codeIndex();
                            lastCaller = caller;
                        }
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
                }
            }
            System.out.println("  hits from mid: " + hits);
            System.out.println("  last hit in: " + lastIn);
            System.out.println("  its caller: " + lastCaller);

            System.out.println("== end");
            if (!ended) {
                erm.deleteEventRequest(bp);
            }
            while (!ended) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    break;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        ended = true;
                    }
                }
                if (!ended) {
                    set.resume();
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
