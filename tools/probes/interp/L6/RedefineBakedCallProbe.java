// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 22, lane L6: a COMPILED caller that is already
// running when its callee's class is retransformed reaches the NEW callee on
// its next call, whatever shape of call it baked
// (docs/internal/fixed-bugs/interpreter-L6-baked-calls-outside-retire-cells-reach-a-redefined-callees-old-body-FIXED-20260926.md):
//
//   shape 1  a plain invokestatic outside any loop (a method-entry body's
//            direct CALL, no retire cell): Caller.calls -> Callee.value
//   shape 2  an invokespecial of a private method: Target.twice -> own
//            (Target itself is retransformed too; its running activation is
//            obsolete and keeps its own constants, JEP 109, but its call
//            reaches the new `own`)
//   shape 2b an invokevirtual on a final class (a devirtualised direct bind):
//            Caller.calls -> Leaf.value
//   shape 3  the same calls from an optimizing-tier body: the 60 000 warm
//            calls are meant to reach the second tier.
//
// Every callee loops so that neither tier splices it (a splice is shape 4,
// fixed in wave 23 and probed by RedefineRunningSpliceProbe.java).
// The transformer renames every "QqA" in Callee's, Target's and Leaf's class
// bytes to "QqB" (same length, same constant-pool indices).
//
// HotSpot 25 prints (with the agent):
//     warm=cQqA|lQqA -> cQqA|lQqA ; pQqA -> pQqA
//     calls=cQqA|lQqA -> cQqB|lQqB
//     twice=pQqA -> pQqB
//     after=cQqB|lQqB -> cQqB|lQqB ; pQqB -> pQqB
// Before wave 22 CratonVM with the JIT on could print cQqA / lQqA / pQqA
// after the arrow on the calls= and twice= lines: the parked compiled caller's
// baked CALL still entered the callee's old compiled body. Since wave 22 a
// redefinition makes those bodies NOT ENTRANT (jit/src/not_entrant.rs): their
// entry jumps to a stub that re-dispatches through the VM. Run with and
// without --nojit, and under --compatible and the default; the output must
// not change. `CRATONVM_DBG_JITC=1` prints one `[cratonvm-jitc] not-entrant:`
// line per patched body (the positive control: with the JIT on it names
// Callee.value, Leaf.value and Target.own), and a build with the `const`
// NOT_ENTRANT_ON_REDEFINITION_ENABLED (vm/src/vm/realms/jit_realm.rs) set to
// false restores the pre-wave-22 behaviour for an A/B.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineBakedCallProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineBakedCallProbe*.class, then run
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar RedefineBakedCallProbe
// Without the agent (the plain probe runner) both VMs print warm= as above,
// then "no agent", and QqA everywhere.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineBakedCallProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Callee {
        public static String value(int n) {
            String s = "cQqA";
            for (int i = 0; i < n; i++) {
                s = s.concat("").intern();
            }
            return s;
        }
    }

    public static final class Leaf {
        public String value(int n) {
            String s = "lQqA";
            for (int i = 0; i < n; i++) {
                s = s.concat("").intern();
            }
            return s;
        }
    }

    public static class Target {
        private String own(int n) {
            String s = "pQqA";
            for (int i = 0; i < n; i++) {
                s = s.concat("").intern();
            }
            return s;
        }

        public String twice(CountDownLatch parked, CountDownLatch go) throws InterruptedException {
            String before = own(3);
            parked.countDown();
            go.await();
            String after = own(3);
            return before + " -> " + after;
        }
    }

    public static class Caller {
        public static String calls(Leaf leaf, CountDownLatch parked, CountDownLatch go)
                throws InterruptedException {
            String before = Callee.value(3) + "|" + leaf.value(3);
            parked.countDown();
            go.await();
            String after = Callee.value(3) + "|" + leaf.value(3);
            return before + " -> " + after;
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineBakedCallProbe$Callee".equals(className)
                    && !"RedefineBakedCallProbe$Target".equals(className)
                    && !"RedefineBakedCallProbe$Leaf".equals(className)) {
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

    static String both(Leaf leaf, Target target, CountDownLatch open) throws InterruptedException {
        return Caller.calls(leaf, open, open) + " ; " + target.twice(open, open);
    }

    public static void main(String[] args) throws Exception {
        CountDownLatch open = new CountDownLatch(0);
        Leaf leaf = new Leaf();
        Target target = new Target();
        String warm = "";
        for (int i = 0; i < 60_000; i++) {
            warm = both(leaf, target, open);
        }
        System.out.println("warm=" + warm);

        CountDownLatch parkedA = new CountDownLatch(1);
        CountDownLatch parkedB = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        String[] result = new String[2];
        Thread a = new Thread(() -> {
            try {
                result[0] = Caller.calls(leaf, parkedA, go);
            } catch (Throwable t) {
                result[0] = t.toString();
            }
        });
        Thread b = new Thread(() -> {
            try {
                result[1] = target.twice(parkedB, go);
            } catch (Throwable t) {
                result[1] = t.toString();
            }
        });
        a.start();
        b.start();
        parkedA.await();
        parkedB.await();
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(Callee.class, Target.class, Leaf.class);
        } else {
            System.out.println("no agent");
        }
        go.countDown();
        a.join();
        b.join();
        System.out.println("calls=" + result[0]);
        System.out.println("twice=" + result[1]);
        System.out.println("after=" + both(leaf, target, open));
    }
}
