// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 17, lane L5: after a retransform that renumbers
// what a class's constant-pool entries name, its call and field sites must
// resolve the NEW entries on the very next execution (docs/known-issues/
// interpreter/i14-L3-proposal-class-scoped-redefinition-invalidation-20260925.md,
// "Progress (wave 17)", stage 2a: the per-thread site caches now survive a
// redefinition and are retired by the resolution epoch instead of a
// process-wide latch).
//
// `Target.sum` reads `h.pickAf`, calls `h.pickA()` and `Helper.spickA()`. A
// transformer rewrites the UTF-8 "pickA" to "pickB" in Target's class bytes
// (same length, so the class file stays well formed), so after the retransform
// the same constant-pool indices name `pickBf`, `pickB()` and `spickB()`.
// Target is warmed first, so every site is cached (and, with the JIT on,
// compiled) before the retransform.
//
// HotSpot 25 prints (with the agent):
//     before=111
//     after=222
//     again=222
// A stale site cache would print after=111 (or a mix such as 112 / 211).
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineSiteCacheProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineSiteCacheProbe*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineSiteCacheProbe
// Without the agent (the plain probe runner) it prints before=111, then
// "no agent" and after=111 / again=111 on both VMs.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class RedefineSiteCacheProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Helper {
        public int pickAf = 100;
        public int pickBf = 200;

        public int pickA() {
            return 1;
        }

        public int pickB() {
            return 2;
        }

        public static int spickA() {
            return 10;
        }

        public static int spickB() {
            return 20;
        }
    }

    public static class Target {
        public static int sum(Helper h) {
            return h.pickAf + h.pickA() + Helper.spickA();
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineSiteCacheProbe$Target".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            byte[] from = {'p', 'i', 'c', 'k', 'A'};
            for (int i = 0; i + from.length <= out.length; i++) {
                boolean match = true;
                for (int j = 0; j < from.length; j++) {
                    if (out[i + j] != from[j]) {
                        match = false;
                        break;
                    }
                }
                if (match) {
                    out[i + 4] = 'B';
                }
            }
            return out;
        }
    }

    static int warm(Helper h, int n) {
        int last = 0;
        for (int i = 0; i < n; i++) {
            last = Target.sum(h);
        }
        return last;
    }

    public static void main(String[] args) throws Exception {
        Helper h = new Helper();
        System.out.println("before=" + warm(h, 50_000));
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(Target.class);
        } else {
            System.out.println("no agent");
        }
        System.out.println("after=" + Target.sum(h));
        System.out.println("again=" + warm(h, 50_000));
    }
}
