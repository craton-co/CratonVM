// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 24, lane L3
// (case 2 of
// docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md,
// "a history past its hard caps"): L3ObsoleteParkedAcrossRenames with 600
// renames instead of 150. A thread stays parked inside Target.parked while
// its class is retransformed 600 times, each retransform renaming the
// literal "parkQqA" in place to a DIFFERENT string, so every redefinition
// gives the parked frame's `ldc` index another string and no two history
// steps share a translation. A blocked thread moves its frames only when it
// wakes, so the frame then needs all 600 steps.
//
// CratonVM in wave 23 kept such steps up to 128 KiB per class, and each
// translation was a dense map as long as the merged pool, which grows by two
// constants per rename: 600 renames are some 800 KiB of translations, so the
// first steps were dropped and the woken frame read the NEW pool at its old
// index (read from the code, not run: parked=parkQqA,parkQxC -- the last
// rename). Since wave 24 a translation is kept as runs (a few per rename),
// so the history holds all 600 steps in a few tens of KiB.
//
// Wave 25, lane L3: the wave-24 build still printed parked=parkQqA,parkQxC
// in the default (--jdk-only) mode, JIT on and --nojit, and was right under
// --compatible. CratonVM's retransformClasses redefines the class TWICE
// (back to the retransformation base, then to the transformer's output), and
// both halves move the renamed constant, so 600 retransforms are 1,199
// history steps -- past the 1,024-step hard cap, which dropped the parked
// frame's first steps. --compatible hid it: its CountDownLatch.await is a
// native that waits in 10 ms slices, and each slice's exit moves the frame,
// so it never needs more than a few steps; the real JDK's latch parks once.
// The hard caps now count each step's record and allow 4,096 steps / 512 KiB
// (Rust test obsolete_frames::tests::
// a_thread_parked_across_six_hundred_retransforms_stays_translatable).
//
// HotSpot 25 prints (with the agent; -Xint too):
//     renames=600 parked=parkQqA,parkQqA
//     second=parkQxC,parkQxC
// The second line parks a new worker on the class's 600th version
// ("parkQxC") across 20 more renames.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3W24ParkedAcrossManyRenames$Agent
//     Can-Retransform-Classes: true
// containing L3W24ParkedAcrossManyRenames*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W24ParkedAcrossManyRenames
// Without the agent both VMs print "no agent" and then
//     renames=600 parked=parkQqA,parkQqA
//     second=parkQqA,parkQqA
// The worker is given 200 ms to park instead of polling Thread.getState().
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3W24ParkedAcrossManyRenames {
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
                    || !"L3W24ParkedAcrossManyRenames$Target".equals(className)) {
                return null;
            }
            int n = ++renames;
            byte[] out = bytes.clone();
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 1] = (byte) ('a' + (n / 26) % 26);
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
        System.out.println("renames=600 parked=" + run(600));
        System.out.println("second=" + run(20));
    }
}
