// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 53, lane L2: shape 2 (c) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md
// -- an optimizing METHOD-ENTRY body of a `synchronized` method whose graph
// can neither trap nor call kept verdict-blind back-edge polls, so its running
// activation finished compiled whatever the VM asked. Built: the wave-25
// design of
// docs/internal/fixed-bugs/interpreter-L2-proposal-caller-held-site-resumes-a-callee-that-left-FIXED-20261017.md
// (`CRATONVM_JIT_IR_SYNC_LEAF_NAMED_POLL_EXITS`): such a body's polls leave on
// `SAFEPOINT_VERDICT_NAMED_BODY`, which the VM never sets for a body a
// compiled caller bound for a caller-held synchronized CALL.
//
// The request here is a class redefinition, the one a plain run can make:
// `spin` loops while the spliced `Tgt.k()` answers 0 (pure arithmetic once
// `k` is inlined: no trap, no call). A second thread redefines `Tgt` so that
// `k()` answers 1. HotSpot deoptimizes the running compiled frame that inlined
// the old `k` (JVMTI RedefineClasses), so the loop ends long before its
// limit. Before wave 53 CratonVM's optimizing body of `spin` ran its splice of
// the OLD `k` to the limit. `spin` is called only through reflection, so no
// compiled caller binds it for a caller-held CALL (a bound body keeps the old
// shape; see the page). Run twice: the first round is the cold-VM case.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     round 1 warm-up=4000000
//     round 1 left early=true
//     round 1 after=0
//     round 2 warm-up=4000000
//     round 2 left early=true
//     round 2 after=0
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W53SyncLeafLoopLeavesAtRedefinition$Agent
//     Can-Redefine-Classes: true
// containing L2W53SyncLeafLoopLeavesAtRedefinition*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W53SyncLeafLoopLeavesAtRedefinition
// Without the agent both VMs print "no agent".
//
// Positive control: CRATONVM_DBG_IR_LINEAR_SCAN=1 prints, for `spin`'s
// optimizing method-entry compile,
//     [ir-ls] entry named-only poll mode exits: given=1 | refused: ...
// and CRATONVM_DBG_DEOPT=1 prints
//     [cratonvm-deopt] withdrawn body told to leave: ... verdict=0x6
// (or 0x7): bit 0x4 is the named-body bit. With the switch off (`=0`) no
// named-only line is printed and, when the optimizing tier compiled `spin`,
// `left early=false`. A run whose `spin` never reached the optimizing tier
// prints `true` on the base as well (its single-pass body has had exits since
// wave 15): judge the switch by the positive control.
//
// By hand, under a debugger (the page's own case): `L2W22SyncMethodLoopExit`'s
// jdb recipe with the breakpoint in `pureSum`'s loop instead of `divLoop`'s.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;

public class L2W53SyncLeafLoopLeavesAtRedefinition {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Tgt {
        public static int k() {
            return 0;
        }
    }

    static final long LIMIT = 3_000_000_000L;
    static volatile boolean started;
    static volatile long result;

    /**
     * Trap-free and call-free once `Tgt.k()` is inlined. The hash keeps a
     * compiler from replacing the loop by its trip count.
     */
    static synchronized long spin(long limit) {
        long n = 0;
        long h = 1;
        while (n < limit && Tgt.k() == 0) {
            h = h * 0x9E3779B97F4A7C15L + n;
            n++;
        }
        return h == 42 ? -n : n;
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W53SyncLeafLoopLeavesAtRedefinition.class
                .getResourceAsStream("L2W53SyncLeafLoopLeavesAtRedefinition$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `bytes` with `k`'s code (code_length 2: `iconst_0; ireturn`) answering `value`. */
    static byte[] answering(byte[] bytes, int value) {
        byte[] b = bytes.clone();
        int found = 0;
        for (int i = 0; i + 5 < b.length; i++) {
            if (b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 0 && b[i + 3] == 2
                    && (b[i + 4] == 0x03 || b[i + 4] == 0x04) && b[i + 5] == (byte) 0xAC) {
                b[i + 4] = (byte) (0x03 + value);
                found++;
            }
        }
        if (found != 1) {
            throw new IllegalStateException("k's code found " + found + " times");
        }
        return b;
    }

    static long call(Method m, long limit) throws Exception {
        return (Long) m.invoke(null, limit);
    }

    static void round(int r, Instrumentation i, Method m, byte[] tgt) throws Exception {
        long w = 0;
        for (int k = 0; k < 20_000; k++) {
            w += call(m, 200);
        }
        System.out.println("round " + r + " warm-up=" + w);
        started = false;
        result = -1;
        Thread worker = new Thread(() -> {
            started = true;
            try {
                result = call(m, LIMIT);
            } catch (Exception e) {
                result = -2;
            }
        }, "spinner");
        worker.setDaemon(true);
        worker.start();
        while (!started) {
            Thread.onSpinWait();
        }
        Thread.sleep(300);
        i.redefineClasses(new ClassDefinition(Tgt.class, answering(tgt, 1)));
        worker.join(120_000);
        long n = result;
        System.out.println("round " + r + " left early=" + (n >= 0 && n < LIMIT));
        System.out.println("round " + r + " after=" + call(m, 5));
        i.redefineClasses(new ClassDefinition(Tgt.class, answering(tgt, 0)));
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] tgt = bytesOf("Tgt");
        Method m = L2W53SyncLeafLoopLeavesAtRedefinition.class.getDeclaredMethod("spin", long.class);
        round(1, i, m, tgt);
        round(2, i, m, tgt);
    }
}
