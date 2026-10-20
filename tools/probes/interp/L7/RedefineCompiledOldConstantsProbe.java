// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 20, lane L1: a COMPILED activation that is
// running when its class is retransformed keeps running the original
// bytecode, and that bytecode's constant-pool indices keep naming the
// original constants (JEP 109 obsolete methods), also at an `ldc` site it
// first executes after the retransform
// (docs/internal/fixed-bugs/interpreter-L3-compiled-bodies-of-a-redefined-class-resolve-old-indices-in-the-new-pool-RETIRED-20261003.md).
// The interpreted half is L7ObsoleteMethodConstants.
//
// `Target.parked` is called 30 000 times with released latches and
// `rare == false` first, so it is compiled (with the JIT on) and its
// `"rareQqA"` site has never run: a compiled body cannot have baked that
// constant and resolves it through `jit_ldc_string_cp` when it first runs.
// Then a worker parks inside it, a transformer renames every "QqA" in
// Target's class bytes to "QqB" (same length: the same constant-pool indices
// now name "...QqB" strings), and the worker resumes with `rare == true`.
//
// HotSpot 25 prints (with the agent):
//     warm=parkQqA,parkQqA
//     parked=parkQqA,rareQqA
//     after=parkQqB,rareQqB
// CratonVM with the JIT on printed parked=parkQqA,rareQqB before wave 20: the
// compiled body resolved the old index in the new pool. Run with and without
// --nojit; the output must not change.
//
// Wave 20 printed after=parkQqA,rareQqA with the JIT on: the call after
// main's loop, made from main's running OSR body, was a plain baked CALL
// into the old compiled Target.parked. Since wave 21 every invokestatic
// site of an OSR body goes through a retire cell, which the redefinition
// clears (docs/internal/fixed-bugs/interpreter-L0-a-running-compiled-caller-keeps-calling-a-redefined-callees-old-body-FIXED-20260925.md).
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineCompiledOldConstantsProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineCompiledOldConstantsProbe*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineCompiledOldConstantsProbe
// Without the agent (the plain probe runner) both VMs print warm= as above,
// then "no agent", parked=parkQqA,rareQqA and after=parkQqA,rareQqA.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineCompiledOldConstantsProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String parked(CountDownLatch parked, CountDownLatch go, boolean rare)
                throws InterruptedException {
            String before = "parkQqA";
            parked.countDown();
            go.await();
            String after = before;
            if (rare) {
                after = "rareQqA";
            }
            return before + "," + after;
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineCompiledOldConstantsProbe$Target".equals(className)) {
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
        CountDownLatch open = new CountDownLatch(0);
        String warm = "";
        for (int i = 0; i < 30_000; i++) {
            warm = Target.parked(open, open, false);
        }
        System.out.println("warm=" + warm);

        CountDownLatch parked = new CountDownLatch(1);
        CountDownLatch go = new CountDownLatch(1);
        String[] result = new String[1];
        Thread worker = new Thread(() -> {
            try {
                result[0] = Target.parked(parked, go, true);
            } catch (Throwable t) {
                result[0] = t.toString();
            }
        });
        worker.start();
        parked.await();
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(Target.class);
        } else {
            System.out.println("no agent");
        }
        go.countDown();
        worker.join();
        System.out.println("parked=" + result[0]);
        System.out.println("after=" + Target.parked(open, open, true));
    }
}
