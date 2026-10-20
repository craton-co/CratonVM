// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 22, lane L3: a VIRTUAL thread parked inside
// Target.parked (unmounted, its Java frames kept in its continuation) while
// Target is retransformed so the index the frame's `ldc` names holds another
// string. On resume the frame must still load its own constant (JEP 109
// obsolete method).
//
// CratonVM moves a thread's frames onto their translated copies at a
// safepoint poll or a blocking-region exit; the redefinition's handshake pause
// reaches only mounted threads, and a remounted continuation
// (`interpreter::resume_continuation`) polls only when a pause is requested.
// Since wave 22 `resume_continuation` converts the resumed frames first
// (`obsolete_frames::convert_thawed_frames`); before, the frame could run on
// against the new pool (read from the code, not run: parked=parkQqA,parkQqB
// when the virtual thread really unmounts in `await`).
//
// HotSpot 25 prints (with the agent):
//     parked=parkQqA,parkQqA
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L3ObsoleteVirtualThreadResume$Agent
//     Can-Retransform-Classes: true
// containing L3ObsoleteVirtualThreadResume*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3ObsoleteVirtualThreadResume
// Without the agent both VMs print "no agent" and the same line.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L3ObsoleteVirtualThreadResume {
    static volatile Instrumentation inst;

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

    /** Renames QqA to QqB in Target, in place. */
    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null
                    || !"L3ObsoleteVirtualThreadResume$Target".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            for (int i = 0; i + 2 < out.length; i++) {
                if (out[i] == 'Q' && out[i + 1] == 'q' && out[i + 2] == 'A') {
                    out[i + 2] = 'B';
                }
            }
            return out;
        }
    }

    public static void main(String[] args) throws Exception {
        if (inst == null) {
            System.out.println("no agent");
        } else {
            inst.addTransformer(new Rename(), true);
        }
        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        String[] result = new String[1];
        Thread worker = Thread.ofVirtual().start(() -> {
            try {
                result[0] = Target.parked(parked, go);
            } catch (Throwable t) {
                result[0] = t.toString();
            }
        });
        parked.await();
        while (worker.getState() != Thread.State.WAITING) {
            Thread.onSpinWait();
        }
        if (inst != null) {
            inst.retransformClasses(Target.class);
        }
        go.countDown();
        worker.join();
        System.out.println("parked=" + result[0]);
    }
}
