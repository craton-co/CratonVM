// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 22, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-untranslatable-obsolete-frames-keep-reading-the-new-pool-RETIRED-20260930.md,
// case 2): a thread stays parked inside Target.parked while its class is
// retransformed TOGGLES times, each retransform renaming "QqA" to "QqB" in
// place (odd ones) or handing the original bytes back (even ones), so every
// one of them puts another string at the index the parked frame's `ldc`
// names. A blocked thread moves its frames only when it wakes, so the frame
// then needs every one of those redefinitions to translate its constant.
// CratonVM kept the last eight: from the ninth on the frame was left as it
// was and read the NEW pool at its old index. Since wave 22 a class's history
// keeps steps while they fit a byte budget, and the toggles share their two
// translations.
//
// HotSpot 25 prints (with the agent):
//     toggles=9 parked=parkQqA,parkQqA
//     toggles=41 parked=parkQqA,parkQqA
// CratonVM before wave 22 (read from the code, not run) printed
// parked=parkQqA,parkQqB on both lines: an odd count leaves the class on the
// QqB version, whose pool names "parkQqB" at the frame's old index. More
// than MAX_HISTORY_STEPS (256) such toggles would still strand the frame.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3ObsoleteParkedAcrossToggles$Agent
//     Can-Retransform-Classes: true
// containing L3ObsoleteParkedAcrossToggles*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3ObsoleteParkedAcrossToggles
// Without the agent both VMs print "no agent" and the same two lines.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3ObsoleteParkedAcrossToggles {
    static volatile Instrumentation inst;
    static int transforms;

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

    /** Odd retransforms rename QqA to QqB; even ones keep the original. */
    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !"L3ObsoleteParkedAcrossToggles$Target".equals(className)) {
                return null;
            }
            transforms++;
            byte[] out = bytes.clone();
            if (transforms % 2 == 0) {
                return out;
            }
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 2] = 'B';
                }
            }
            return out;
        }
    }

    static String run(int toggles) throws Exception {
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
        while (worker.getState() != Thread.State.WAITING) {
            Thread.onSpinWait();
        }
        if (inst != null) {
            for (int i = 0; i < toggles; i++) {
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
        System.out.println("toggles=9 parked=" + run(9));
        // Back on the original bytes before the next round.
        if (inst != null && transforms % 2 == 1) {
            inst.retransformClasses(Target.class);
        }
        System.out.println("toggles=41 parked=" + run(41));
    }
}
