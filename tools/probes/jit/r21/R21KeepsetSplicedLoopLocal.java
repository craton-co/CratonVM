// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 1, lane keepset: residual 3 of
// docs/known-issues/jit/gci-i40os-ir-keep-set-shared-colour-residuals-20261016.md.
//
// A loop INSIDE a spliced callee leaves for the interpreter at its back-edge
// poll through a caller chain (CRATONVM_JIT_IR_SPLICED_HEADER_POLL_EXITS /
// ..._ENTRY_SPLICED_HEADER_POLL_EXITS, default ON). That mode exit reads the
// callee's locals from the chain snapshot recorded at the loop header. `b`
// below is a callee local the loop reads only through `b == t`, a
// loop-invariant compare the optimizer may hoist to the pre-header; then no
// node inside the loop names `b`, the keep set called it dead at the poll, the
// poll's slow path zeroed its home, and the exit rebuilt `sum`'s frame with
// `b == null`. A JDWP suspension is what sends the compiled loop to the
// interpreter at its next poll (I41JdiSuspendStopsEveryLoopShape), so the
// debugger reads `b` from the rebuilt frame.
//
// Rows (canonical: no ids, no addresses, no timings):
//   == attach
//     top frame: sum
//     b null: false
//     b.v: 7
//     t null: false
//     t.v: 7
//   == end
//     disconnected
//
// HotSpot 25 as the debuggee prints exactly the rows above (C2 keeps `b` in
// the debug info of the loop's safepoint: it is live in the bytecode). The
// defect prints `b null: true` and no `b.v` row. If the compare is not hoisted
// on a build, `b` stays read inside the loop and both arms print the HotSpot
// rows (the probe then does not reach the shape; check with
// CRATONVM_DBG_IR_SLOTS=1 for an `[ir-slots] spliced latch exit reads` line).
//
// Plain run (no debugger): `ended`.
//
// Run (build with --features experimental-debug for --jdwp-port):
//   javac -g -d out R21KeepsetSplicedLoopLocal.java
//   cratonvm --java-home $JDK --jdwp-port 5793 -Xmx512m -cp out R21KeepsetSplicedLoopLocal wait
//   java -cp out R21KeepsetSplicedLoopLocal debug 5793
// Arms: default (fixed) and CRATONVM_JIT_IR_SPLICED_LATCH_EXIT_READS=0 (the
// pre-fix liveness, expected `b null: true` when the shape is reached). Also
// the control CRATONVM_JIT_IR_KEEP_SET_SPLICED_EXITLESS_STEPS=0 with the fix
// switch at 0 (the header φs read the snapshot again: HotSpot rows).
// HotSpot debuggee:
//   java -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=5793 -cp out R21KeepsetSplicedLoopLocal wait
import com.sun.jdi.*;
import com.sun.jdi.connect.*;
import java.util.*;

public class R21KeepsetSplicedLoopLocal {
    static volatile int phase;
    static volatile long sink;

    static final class Box {
        final int v;

        Box(int v) {
            this.v = v;
        }
    }

    // BOXES[0] and BOXES[2] are the same object: `b == t` is true at run time
    // and not provable at compile time.
    static final Box SEVEN = new Box(7);
    static final Box[] BOXES = {SEVEN, new Box(9), SEVEN};

    // Small enough to splice. `b` is read in the loop only by the invariant
    // compare; `t` is also read after the loop (its home stays live there).
    static long sum(Box[] boxes, long n) {
        Box b = boxes[0];
        Box t = boxes[2];
        long acc = 0;
        for (long i = 0; i < n; i++) {
            acc = acc * 31 + (b == t ? 1 : 2);
        }
        return acc + t.v;
    }

    static void spin() {
        long acc = 0;
        for (int i = 0; i < 20_000; i++) {
            acc += sum(BOXES, 64);
        }
        sink = acc;
        phase = 2;
        // Never returns: the debugger reads the loop's frame.
        sink = sum(BOXES, Long.MAX_VALUE);
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 2 && args[0].equals("debug")) {
            Debugger.run(Integer.parseInt(args[1]));
            return;
        }
        boolean debugged = args.length == 1 && args[0].equals("wait");
        Thread t = new Thread(R21KeepsetSplicedLoopLocal::spin, "spinner");
        t.setDaemon(true);
        t.start();
        long until = System.currentTimeMillis() + 300_000;
        if (debugged) {
            while (phase != 3 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
        } else {
            while (phase != 2 && System.currentTimeMillis() < until) {
                Thread.sleep(10);
            }
            Thread.sleep(500);
        }
        System.out.println("ended");
    }

    static final class Debugger {
        static void local(StackFrame f, String name, ReferenceType box) throws Exception {
            LocalVariable var = f.visibleVariableByName(name);
            if (var == null) {
                System.out.println("  " + name + " missing (compile with javac -g)");
                return;
            }
            Value v = f.getValue(var);
            System.out.println("  " + name + " null: " + (v == null));
            if (v instanceof ObjectReference) {
                Value field = ((ObjectReference) v).getValue(box.fieldByName("v"));
                System.out.println("  " + name + ".v: " + field);
            }
        }

        static void run(int port) throws Exception {
            AttachingConnector conn = null;
            for (AttachingConnector c : Bootstrap.virtualMachineManager().attachingConnectors()) {
                if (c.transport().name().equals("dt_socket")) {
                    conn = c;
                }
            }
            Map<String, Connector.Argument> a = conn.defaultArguments();
            a.get("port").setValue(Integer.toString(port));
            a.get("hostname").setValue("localhost");
            VirtualMachine vm = conn.attach(a);
            System.out.println("== attach");
            ReferenceType main = vm.classesByName("R21KeepsetSplicedLoopLocal").get(0);
            Field phase = main.fieldByName("phase");
            long until = System.currentTimeMillis() + 120_000;
            while (((IntegerValue) main.getValue(phase)).value() != 2
                    && System.currentTimeMillis() < until) {
                Thread.sleep(20);
            }
            // Let the long loop run hot: a compiled body, not the interpreter.
            Thread.sleep(3000);
            ThreadReference spinner = null;
            for (ThreadReference t : vm.allThreads()) {
                if (t.name().equals("spinner")) {
                    spinner = t;
                }
            }
            ReferenceType box = vm.classesByName("R21KeepsetSplicedLoopLocal$Box").get(0);
            vm.suspend();
            StackFrame top = spinner.frame(0);
            System.out.println("  top frame: " + top.location().method().name());
            local(top, "b", box);
            local(spinner.frame(0), "t", box);
            vm.resume();
            ((ClassType) main).setValue(phase, vm.mirrorOf(3));
            System.out.println("== end");
            vm.dispose();
            System.out.println("  disconnected");
        }
    }
}
