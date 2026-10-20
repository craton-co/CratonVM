// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 54, lane L2: what remained of shape 2 (c) of
// docs/internal/fixed-bugs/i41-L1-compiled-bodies-without-an-exit-finish-their-activation-compiled-FIXED-20261007.md
// after wave 53 -- the optimizing METHOD-ENTRY body of a `synchronized`,
// trap-free, call-free method that a compiled caller BOUND for a caller-held
// synchronized CALL kept verdict-blind polls in EVERY activation, including
// the ones a VM door entered (the wave-53 mark is per artifact). Built: the
// per-activation question of
// docs/internal/fixed-bugs/interpreter-L2-proposal-a-bound-sync-leaf-body-asks-whether-its-caller-holds-the-monitor-FIXED-20261018.md
// (`CRATONVM_JIT_SYNC_LEAF_DOOR_ACTIVATION_EXITS`): an activation the door
// entered -- the thread's innermost JIT entry names the body -- gets the
// named-body bit, as an unbound body's does.
//
// `L2W53SyncLeafLoopLeavesAtRedefinition` with a compiled caller added to
// the warm-up: `bindMany` calls `spin` directly and often enough to be
// compiled, which binds `spin` for a caller-held CALL when the tiering race
// lets it. The measured call is made through reflection (a door). HotSpot
// deoptimizes the running compiled frame that inlined the old `Tgt.k()` (JVMTI
// RedefineClasses), so the loop ends long before its limit. Run twice: the
// first round is the cold-VM case.
//
// HotSpot 25 prints (agent; the same with -Xint):
//     round 1 warm-up=4000000
//     round 1 bound-warm-up=4000000
//     round 1 left early=true
//     round 1 after=0
//     round 2 warm-up=4000000
//     round 2 bound-warm-up=4000000
//     round 2 left early=true
//     round 2 after=0
//
// SETUP: a jar whose manifest has
//     Premain-Class: L2W54BoundSyncLeafLeavesFromADoor$Agent
//     Can-Redefine-Classes: true
// containing L2W54BoundSyncLeafLeavesFromADoor*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L2W54BoundSyncLeafLeavesFromADoor
// Without the agent both VMs print "no agent".
//
// Positive control: CRATONVM_DBG_JITC=1 prints a `sync-direct` line binding
// `spin` for `bindMany`'s compile (the body is then marked bound), and
// CRATONVM_DBG_DEOPT=1 prints
//     [cratonvm-deopt] withdrawn body told to leave: ...spin... verdict=0x6
// (or 0x7): bit 0x4 is the named-body bit, which a bound body gets only from
// this wave's per-activation answer. With the switch off (`=0`) and `spin`
// bound, the verdict has no 0x4 bit and `left early=false`. A run where no
// compiled caller bound `spin` prints `true` on the base too (wave 53):
// judge the switch by the positive control.
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.Instrumentation;
import java.lang.reflect.Method;

public class L2W54BoundSyncLeafLeavesFromADoor {
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

    /** A compiled caller of `spin`: its compile may bind it caller-held. */
    static long bindMany(int calls) {
        long s = 0;
        for (int c = 0; c < calls; c++) {
            s += spin(200);
        }
        return s;
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L2W54BoundSyncLeafLeavesFromADoor.class
                .getResourceAsStream("L2W54BoundSyncLeafLeavesFromADoor$" + simple + ".class")) {
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
        long b = 0;
        for (int k = 0; k < 200; k++) {
            b += bindMany(100);
        }
        System.out.println("round " + r + " bound-warm-up=" + b);
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
        Method m = L2W54BoundSyncLeafLeavesFromADoor.class.getDeclaredMethod("spin", long.class);
        round(1, i, m, tgt);
        round(2, i, m, tgt);
    }
}
