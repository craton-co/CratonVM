// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 19, lane L3: a frame that is running when its
// class is redefined keeps running the ORIGINAL bytecode (JVMTI
// RedefineClasses / JEP 109), and that bytecode's constant-pool indices keep
// naming the ORIGINAL constants (HotSpot runs it as an obsolete method with
// the old constants). CratonVM used to resolve the old indices in the NEW
// pool (docs/internal/fixed-bugs/
// interpreter-L3-obsolete-methods-keep-no-constant-pool-FIXED-20260925.md).
//
// A transformer renames every "QqA" in Target's class bytes to "QqB" (same
// length, so the class file stays well formed): the same constant-pool
// indices then name "...QqB" strings, `Helper.fQqB` and `Helper.mQqB()`. The
// next retransform hands the original bytes back unchanged.
//
//   * parked: a thread is inside Target.parked (waiting on a latch) when the
//     class is renamed; after the wait the same method loads its string
//     constant again and reads a field and calls a method through the pool.
//   * self:   Target.selfRedefine (already renamed) retransforms its own class
//     back to the original, then does the same.
//
// HotSpot 25 prints (with the agent):
//     before=valQqA,1,10
//     parked=parkQqA,parkQqA,1,10
//     after=valQqB,2,20
//     self=selfQqB,selfQqB,2,20
//     again=valQqA,1,10
// CratonVM before wave 19 printed parked=parkQqA,parkQqB,2,20 and
// self=selfQqB,selfQqA,1,10: the running frames read the replaced pool.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: L7ObsoleteMethodConstants$Agent
//     Can-Retransform-Classes: true
// containing L7ObsoleteMethodConstants*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar L7ObsoleteMethodConstants
// Without the agent (the plain probe runner) both VMs print before= as above,
// then "no agent" and every other line with the QqA values
// (parked=parkQqA,parkQqA,1,10, after=valQqA,1,10, self=selfQqA,selfQqA,1,10,
// again=valQqA,1,10).
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class L7ObsoleteMethodConstants {
    static volatile Instrumentation inst;
    static int transforms;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Helper {
        public static int fQqA = 1;
        public static int fQqB = 2;

        public static int mQqA() {
            return 10;
        }

        public static int mQqB() {
            return 20;
        }
    }

    public static class Target {
        public static String values() {
            return "valQqA," + Helper.fQqA + "," + Helper.mQqA();
        }

        public static String parked(CountDownLatch parked, CountDownLatch go)
                throws InterruptedException {
            String before = "parkQqA";
            parked.countDown();
            go.await();
            String after = "parkQqA";
            return before + "," + after + "," + Helper.fQqA + "," + Helper.mQqA();
        }

        public static String selfRedefine() throws Exception {
            String before = "selfQqA";
            retransformTarget();
            String after = "selfQqA";
            return before + "," + after + "," + Helper.fQqA + "," + Helper.mQqA();
        }
    }

    /** Odd retransforms rename QqA to QqB; even ones hand the original back. */
    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"L7ObsoleteMethodConstants$Target".equals(className)) {
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

    static void retransformTarget() throws Exception {
        if (inst != null) {
            inst.retransformClasses(Target.class);
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("before=" + Target.values());
        if (inst == null) {
            System.out.println("no agent");
        } else {
            inst.addTransformer(new Rename(), true);
        }

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
        retransformTarget();
        go.countDown();
        worker.join();
        System.out.println("parked=" + result[0]);

        System.out.println("after=" + Target.values());
        System.out.println("self=" + Target.selfRedefine());
        System.out.println("again=" + Target.values());
    }
}
