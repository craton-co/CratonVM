// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 21, lane L1: a COMPILED activation that is
// running when its class is retransformed keeps running the original
// bytecode, and its `multianewarray` site keeps naming the original array
// class (JEP 109 obsolete methods), also when it first executes after the
// retransform
// (docs/internal/fixed-bugs/interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED-20260925.md).
// The `ldc` twin is RedefineCompiledOldConstantsProbe.
//
// `Target.parked` is called 30 000 times with released latches and
// `rare == false` first, so it is compiled (with the JIT on) and its
// `new RedefineCompiledMultiArrayProbeQqA[1][1]` site has never run: the
// compiled body resolves it through `jit_multianewarray_n` when it first
// runs. Then a worker parks inside it, a transformer renames every "QqA" in
// Target's class bytes to "QqB" (same length: the same constant-pool index
// now names the sibling array class), and the worker resumes with
// `rare == true`.
//
// HotSpot 25 prints (with the agent):
//     warm=none
//     parked=[[LRedefineCompiledMultiArrayProbeQqA;
//     after=[[LRedefineCompiledMultiArrayProbeQqB;
// CratonVM with the JIT on printed parked=[[LRedefineCompiledMultiArrayProbeQqB;
// before wave 21: the compiled body's multianewarray site read its old index
// in the new pool. Run with and without --nojit; the output must not change.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineCompiledMultiArrayProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineCompiledMultiArrayProbe*.class (the two sibling classes
// below match that pattern), then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineCompiledMultiArrayProbe
// Without the agent (the plain probe runner) both VMs print warm= as above,
// then "no agent", and QqA on both the parked= and the after= line.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.util.concurrent.CountDownLatch;

public class RedefineCompiledMultiArrayProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String parked(CountDownLatch parked, CountDownLatch go, boolean rare)
                throws InterruptedException {
            parked.countDown();
            go.await();
            if (rare) {
                Object[][] grid = new RedefineCompiledMultiArrayProbeQqA[1][1];
                return grid.getClass().getName();
            }
            return "none";
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineCompiledMultiArrayProbe$Target".equals(className)) {
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

class RedefineCompiledMultiArrayProbeQqA {
}

class RedefineCompiledMultiArrayProbeQqB {
}
