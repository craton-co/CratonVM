// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 119, lane L8: a debugger stops at every bytecode
// of the fused groups lane L8 added in waves 115-118, at its own bci:
//
//   `iload{_N, n}; if<cond>`             CRATONVM_INTERP_FUSED_ZERO_BRANCH (on)
//   `aload_N; ifnull | ifnonnull`        CRATONVM_INTERP_FUSED_NULL_BRANCH (on)
//   `aload n; ifnull | ifnonnull`        CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE
//   `getfield; getfield [; ...]`         CRATONVM_INTERP_FUSED_GETFIELD_CHAIN
//   `tableswitch` / `lookupswitch`       CRATONVM_INTERP_FAST_SWITCH_ARMS
//   `iinc; goto` + the loop test         CRATONVM_INTERP_FUSED_LOOP_STEP_TEST
//   the lean loop top                    CRATONVM_INTERP_LEAN_LOOP_TOP
//
// `work` holds every shape and calls nothing. The debugger (HotSpot's JDI)
// runs three phases against the debuggee:
//
//   1. hits: with the thread stopped in `marker()`, a breakpoint on EVERY
//      bytecode of `work`; one call of `work` runs; each bci's hit count is
//      printed. A fused group that ran a bytecode past its breakpoint shows
//      as a low count.
//   2. steps: from `marker()`, STEP_MIN / STEP_INTO single steps through one
//      call of `work`; the number of step events inside `work`, the number of
//      distinct bcis and a hash of the bci sequence are printed.
//   3. live: the debuggee spins in `spin()` (fused loops running); a
//      breakpoint is set WHILE it runs on the interior bci of its `iload;
//      ifne`, on the second getfield of its chain, and on the `if_icmpge` of
//      its loop test (the target of `iinc; goto`); each must be hit, at the
//      requested bci.
//
// Canonical transcript: no ids, no addresses, no timings. Run it with the
// conformance runner, every switch off and then on:
//
//   tools/jdi/run-jdi-conformance.sh --jdk $JDK --cratonvm <debug cratonvm> \
//       --modes "jdk-only jdk-only:nojit compatible:nojit" --scenario L8W119JdiFusedGroupStops
//   (and again with CRATONVM_INTERP_FUSED_NULL_BRANCH_WIDE=1
//    CRATONVM_INTERP_FUSED_GETFIELD_CHAIN=1 CRATONVM_INTERP_FAST_SWITCH_ARMS=1
//    CRATONVM_INTERP_FUSED_LOOP_STEP_TEST=1 CRATONVM_INTERP_LEAN_LOOP_TOP=1 in
//    the debuggee's environment)
//
// By hand:
//   javac -g -d out L8W119JdiFusedGroupStops.java
//   cratonvm --java-home $JDK --jdwp-port 5791 -cp out L8W119JdiFusedGroupStops wait
//   java -cp out L8W119JdiFusedGroupStops debug 5791 > transcript.txt
//
// Plain run (no debugger), stdout must equal HotSpot 25's:
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// work 1602
// HOTSPOT_EXPECTED_END
//
// Under the debugger the transcript is compared against HotSpot's debuggee
// by the runner (the per-bci list depends on javac's code layout). HotSpot
// 25.0.3 as the debuggee, javac 25 (the tail of the transcript):
//
//   never hit: 4                       (the `wide == null` body: wide is non-null)
//   == steps
//     step events in work: 439
//     distinct bcis: 116 of 120
//     sequence hash: 8667414454620920588
//   == live
//     live breakpoint at op 0x9a: hit true, same bci true
//     live breakpoint at op 0xb4: hit true, same bci true
//     live breakpoint at op 0xa2: hit true, same bci true
//   == end
//     disconnected
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import com.sun.jdi.event.*;
import com.sun.jdi.request.*;
import java.util.*;

public class L8W119JdiFusedGroupStops {
    static volatile int stop;
    static volatile int mode;
    static volatile int spinStop;
    static long sink;

    static final class Leaf {
        int c = 3;
    }

    static final class Mid {
        Leaf b = new Leaf();
    }

    static final class Head {
        Mid a = new Mid();
    }

    static final class Node {
        final int v;
        Node next;

        Node(int v) {
            this.v = v;
        }
    }

    static void marker() {
        sink++;
    }

    static void marker2() {
        sink++;
    }

    // n is local 0 (iload_0), h 1, head 2, o 3; s 4-5, wide 6, i 7, k 8, p 9.
    static long work(int n, Head h, Node head, Object o) {
        long s = 0;
        Object wide = o;
        for (int i = 0; i < n; i++) {             // iload 7; iload_0; if_icmpge ... iinc; goto
            if (n != 0) {                         // iload_0; ifeq
                s += 1;
            }
            int k = i & 1;
            if (k == 0) {                         // iload 8; ifne
                s += 2;
            }
            if (o != null) {                      // aload_3; ifnull
                s += 3;
            }
            if (wide == null) {                   // aload 6; ifnonnull
                s += 4;
            }
            s += h.a.b.c;                         // aload_1; getfield x3
            switch (i & 3) {                      // tableswitch
                case 0: s += 10; break;
                case 1: s += 20; break;
                case 2: s += 30; break;
                default: s += 40; break;
            }
            switch (i * 1000) {                   // lookupswitch
                case 0: s += 100; break;
                case 2000: s += 200; break;
                case 4000: s += 300; break;
                default: s += 400; break;
            }
        }
        for (Node p = head; p != null; p = p.next) {  // aload 9; ifnull
            s += p.v;
        }
        int m = 4;
        while (m != 0) {                          // iload; ifeq; iinc; goto
            m--;
            s += m;
        }
        return s;
    }

    // h 0, limit 1, s 2-3, i 4, k 5. The loop test `iload 4; iload_1;
    // if_icmpge` is the target of the `iinc; goto` (the loop step-test).
    static long spin(Head h, int limit) {
        long s = 0;
        for (int i = 0; i < limit; i++) {
            int k = i & 1;
            if (k == 0) {                         // iload; ifne
                s += 1;
            }
            s += h.a.b.c;                         // aload_0; getfield x3
            if (spinStop != 0) {
                break;
            }
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        Head h = new Head();
        Node head = null;
        for (int i = 9; i >= 0; i--) {
            Node p = new Node(i);
            p.next = head;
            head = p;
        }
        Object o = new Object();
        long warm = 0;
        for (int r = 0; r < 3000; r++) {
            warm += work(5, h, head, o);
        }
        if (!debugged) {
            System.out.println("work " + work(5, h, head, o));
            return;
        }
        long until = System.currentTimeMillis() + 300_000;
        while (stop == 0 && System.currentTimeMillis() < until) {
            marker();
            sink += work(5, h, head, o);
            marker2();
            if (mode == 1) {
                sink += spin(h, Integer.MAX_VALUE);
                mode = 0;
            }
            Thread.sleep(1);
        }
        System.out.println("done " + (warm > 0));
    }

    static final class Debugger {
        static VirtualMachine vm;

        static int insnLength(byte[] bc, int pc) {
            int op = bc[pc] & 0xff;
            switch (op) {
                case 0x10: case 0x12: case 0x15: case 0x16: case 0x17: case 0x18: case 0x19:
                case 0x36: case 0x37: case 0x38: case 0x39: case 0x3a: case 0xa9: case 0xbc:
                    return 2;
                case 0x11: case 0x13: case 0x14: case 0x84: case 0xb2: case 0xb3: case 0xb4:
                case 0xb5: case 0xb6: case 0xb7: case 0xb8: case 0xbb: case 0xbd: case 0xc0:
                case 0xc1: case 0xc6: case 0xc7:
                    return 3;
                case 0xc5:
                    return 4;
                case 0xb9: case 0xba: case 0xc8: case 0xc9:
                    return 5;
                case 0xc4:
                    return (bc[pc + 1] & 0xff) == 0x84 ? 6 : 4;
                case 0xaa: {
                    int base = (pc + 4) & ~3;
                    int low = readInt(bc, base + 4);
                    int high = readInt(bc, base + 8);
                    return base + 12 + 4 * (high - low + 1) - pc;
                }
                case 0xab: {
                    int base = (pc + 4) & ~3;
                    int npairs = readInt(bc, base + 4);
                    return base + 8 + 8 * npairs - pc;
                }
                default:
                    if (op >= 0x99 && op <= 0xa8) {
                        return 3;
                    }
                    return 1;
            }
        }

        static int readInt(byte[] bc, int at) {
            return ((bc[at] & 0xff) << 24) | ((bc[at + 1] & 0xff) << 16) | ((bc[at + 2] & 0xff) << 8)
                    | (bc[at + 3] & 0xff);
        }

        static List<Integer> bcis(Method m) {
            byte[] bc = m.bytecodes();
            List<Integer> out = new ArrayList<>();
            for (int pc = 0; pc < bc.length; pc += insnLength(bc, pc)) {
                out.add(pc);
            }
            return out;
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
            EventRequestManager erm = vm.eventRequestManager();
            System.out.println("== attach");
            ReferenceType type = awaitClass("L8W119JdiFusedGroupStops");
            Method marker = type.methodsByName("marker").get(0);
            Method marker2 = type.methodsByName("marker2").get(0);
            Method work = type.methodsByName("work").get(0);
            Method spin = type.methodsByName("spin").get(0);

            BreakpointRequest atMarker = erm.createBreakpointRequest(marker.location());
            atMarker.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
            atMarker.enable();
            BreakpointRequest atMarker2 = erm.createBreakpointRequest(marker2.location());
            atMarker2.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);

            // Phase 1: a breakpoint on every bytecode of `work`.
            ThreadReference main = awaitBreakpoint(marker);
            System.out.println("== hits");
            List<BreakpointRequest> all = new ArrayList<>();
            for (int pc : bcis(work)) {
                BreakpointRequest bp = erm.createBreakpointRequest(work.locationOfCodeIndex(pc));
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                bp.enable();
                all.add(bp);
            }
            atMarker2.enable();
            main.resume();
            TreeMap<Long, Integer> hits = new TreeMap<>();
            while (true) {
                Location at = awaitLocation(BreakpointEvent.class);
                if (at == null) {
                    fail("no breakpoint event");
                }
                if (at.method().equals(marker2)) {
                    break;
                }
                if (at.method().equals(work)) {
                    hits.merge(at.codeIndex(), 1, Integer::sum);
                }
                resumeMain();
            }
            for (BreakpointRequest bp : all) {
                erm.deleteEventRequest(bp);
            }
            atMarker2.disable();
            int missed = 0;
            for (int pc : bcis(work)) {
                int n = hits.getOrDefault((long) pc, 0);
                System.out.println("  bci " + pc + " op 0x" + Integer.toHexString(work.bytecodes()[pc] & 0xff)
                        + " hits " + n);
                if (n == 0) {
                    missed++;
                }
            }
            System.out.println("  never hit: " + missed);
            resumeMain();

            // Phase 2: single steps through one call of `work`.
            main = awaitBreakpoint(marker);
            System.out.println("== steps");
            int inWork = 0;
            long hash = 17;
            TreeSet<Long> distinct = new TreeSet<>();
            for (int k = 0; k < 20_000; k++) {
                StepRequest step = erm.createStepRequest(main, StepRequest.STEP_MIN,
                        StepRequest.STEP_INTO);
                step.addClassFilter("L8W119JdiFusedGroupStops*");
                step.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                step.enable();
                main.resume();
                Location at = awaitLocation(StepEvent.class);
                erm.deleteEventRequest(step);
                if (at == null) {
                    fail("a step never arrived");
                }
                if (at.method().equals(marker2)) {
                    break;
                }
                if (at.method().equals(work)) {
                    inWork++;
                    distinct.add(at.codeIndex());
                    hash = hash * 31 + at.codeIndex();
                }
            }
            System.out.println("  step events in work: " + inWork);
            System.out.println("  distinct bcis: " + distinct.size() + " of " + bcis(work).size());
            System.out.println("  sequence hash: " + hash);

            // Phase 3: breakpoints set while `spin` runs its fused loop.
            System.out.println("== live");
            atMarker.disable();
            ((ClassType) type).setValue(type.fieldByName("mode"), vm.mirrorOf(1));
            main.resume();
            Thread.sleep(1500);
            // The interior bci of the `iload; ifne` group and the second getfield
            // of the `h.a.b.c` chain, found by their bytecode shapes.
            int zeroBci = -1;
            int chainSecond = -1;
            int loopTest = -1;
            byte[] sbc = spin.bytecodes();
            int prev = -1;
            for (int pc : bcis(spin)) {
                int op = sbc[pc] & 0xff;
                int prevOp = prev < 0 ? -1 : sbc[prev] & 0xff;
                if (zeroBci < 0 && op == 0x9a && (prevOp == 0x15 || (prevOp >= 0x1a && prevOp <= 0x1d))) {
                    zeroBci = pc;
                }
                if (chainSecond < 0 && op == 0xb4 && prevOp == 0xb4) {
                    chainSecond = pc;
                }
                if (loopTest < 0 && op == 0xa2 && prevOp == 0x1b) {
                    loopTest = pc;
                }
                prev = pc;
            }
            if (zeroBci < 0 || chainSecond < 0 || loopTest < 0) {
                fail("spin has no fused shapes");
            }
            for (int want : new int[] {zeroBci, chainSecond, loopTest}) {
                BreakpointRequest bp = erm.createBreakpointRequest(spin.locationOfCodeIndex(want));
                bp.setSuspendPolicy(EventRequest.SUSPEND_EVENT_THREAD);
                bp.enable();
                Location at = awaitLocation(BreakpointEvent.class);
                erm.deleteEventRequest(bp);
                System.out.println("  live breakpoint at op 0x"
                        + Integer.toHexString(spin.bytecodes()[want] & 0xff) + ": hit "
                        + (at != null) + ", same bci " + (at != null && at.codeIndex() == want));
                resumeMain();
            }
            ((ClassType) type).setValue(type.fieldByName("spinStop"), vm.mirrorOf(1));
            ((ClassType) type).setValue(type.fieldByName("stop"), vm.mirrorOf(1));
            end();
        }

        static ThreadReference awaitBreakpoint(Method in) throws Exception {
            long until = System.currentTimeMillis() + 60_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                for (Event e : set) {
                    if (e instanceof BreakpointEvent be && be.location().method().equals(in)) {
                        return be.thread();
                    }
                    if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                set.resume();
            }
            fail("no breakpoint in " + in.name());
            return null;
        }

        static EventSet pending;

        static Location awaitLocation(Class<? extends LocatableEvent> kind) throws Exception {
            long until = System.currentTimeMillis() + 30_000;
            while (System.currentTimeMillis() < until) {
                EventSet set = vm.eventQueue().remove(500);
                if (set == null) {
                    continue;
                }
                Location at = null;
                for (Event e : set) {
                    if (kind.isInstance(e)) {
                        at = ((LocatableEvent) e).location();
                    } else if (e instanceof VMDeathEvent || e instanceof VMDisconnectEvent) {
                        fail("the debuggee ended early");
                    }
                }
                if (at != null) {
                    pending = set;
                    return at;
                }
                set.resume();
            }
            return null;
        }

        static void resumeMain() {
            if (pending != null) {
                pending.resume();
                pending = null;
            }
        }

        static void end() throws Exception {
            System.out.println("== end");
            resumeMain();
            while (true) {
                EventSet set;
                try {
                    set = vm.eventQueue().remove(60_000);
                } catch (VMDisconnectedException gone) {
                    System.out.println("  disconnected");
                    return;
                }
                if (set == null) {
                    fail("the debuggee did not end");
                }
                for (Event e : set) {
                    if (e instanceof VMDisconnectEvent) {
                        System.out.println("  disconnected");
                        return;
                    }
                }
                set.resume();
            }
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
