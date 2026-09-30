// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L3
// (docs/known-issues/interpreter/
// i22-L3-proposal-redefinition-history-kept-by-a-stale-frame-census-20260926.md,
// and case 2 of
// docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md):
// a thread stays parked inside Target.parked while its class is
// retransformed RENAMES times, each retransform renaming the literal
// "parkQqA" in place to a DIFFERENT string (parkQaB, parkQaC, ...), so every
// redefinition gives the parked frame's `ldc` index another string and
// renumbers the merged pool differently: no two history steps share a
// translation. A blocked thread moves its frames only when it wakes, so the
// frame then needs every one of those steps.
//
// CratonVM through wave 22 kept a class's history within a 32 KiB byte
// budget (and 256 steps) past its last eight steps, whatever the frames
// needed: 150 such renames are about 54 KiB of translations, so the first
// steps were dropped and the woken frame read the NEW pool at its old index
// (read from the code, not run: parked=parkQqA,parkQfU -- the last rename).
// Since wave 23 a census of the oldest stamp any thread's frames may carry
// keeps the steps a parked frame needs, up to 128 KiB / 1024 steps per class.
//
// HotSpot 25 prints (with the agent):
//     renames=150 parked=parkQqA,parkQqA
//     second=parkQfU,parkQfU
// The second line runs the same shape again after the first worker has
// finished (the class is on its 150th version, "parkQfU", by then) across 20
// more renames: the census lets the first worker's steps go once it has
// moved, and the new parked frame must still translate. CratonVM before wave
// 23 printed the second line right too (20 steps fit the budget).
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3ObsoleteParkedAcrossRenames$Agent
//     Can-Retransform-Classes: true
// containing L3ObsoleteParkedAcrossRenames*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3ObsoleteParkedAcrossRenames
// Without the agent both VMs print "no agent" and the same two lines.
// The worker is given 200 ms to park instead of polling Thread.getState().
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3ObsoleteParkedAcrossRenames {
    static volatile Instrumentation inst;
    static volatile int renames;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String parked(CountDownLatch parked, CountDownLatch go)
                throws InterruptedException {
            String before = "parkQqA";
            parked.countDown();
            go.await();
            String after = "parkQqA";
            return before + "," + after;
        }
    }

    /** Rename QqA to Q + two letters that no earlier retransform used. */
    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !"L3ObsoleteParkedAcrossRenames$Target".equals(className)) {
                return null;
            }
            int n = ++renames;
            byte[] out = bytes.clone();
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 1] = (byte) ('a' + (n / 26) % 16);
                    out[i + 2] = (byte) ('A' + n % 26);
                }
            }
            return out;
        }
    }

    static String run(int count) throws Exception {
        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        String[] result = new String[1];
        Thread worker = new Thread(() -> {
            try {
                result[0] = Target.parked(parked, go);
            } catch (Throwable t) {
                result[0] = t.toString();
            }
        });
        worker.start();
        parked.await();
        Thread.sleep(200);
        if (inst != null) {
            for (int i = 0; i < count; i++) {
                inst.retransformClasses(Target.class);
            }
        }
        go.countDown();
        worker.join();
        return result[0];
    }

    public static void main(String[] args) throws Exception {
        if (inst == null) {
            System.out.println("no agent");
        } else {
            inst.addTransformer(new Rename(), true);
        }
        System.out.println("renames=150 parked=" + run(150));
        System.out.println("second=" + run(20));
    }
}
