// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 18, lane L4: after a retransform that renumbers
// what a CALLER's constant-pool entries name, the caller's `ldc` of a String
// and its `invokespecial` must use the NEW entries on the very next execution,
// interpreted and compiled. Companion of RedefineSiteCacheProbe (wave 17),
// which covers field, virtual and static sites.
//
// What it guards (docs/internal/fixed-bugs/
// interpreter-L5-fast-invoke-doors-decline-for-the-process-after-one-redefinition-FIXED-20260925.md
// and interpreter-L5-jit-helper-memos-latch-off-for-the-process-after-one-redefinition-FIXED-20260925.md):
//   * the per-thread invoke cache keys an entry on (caller class, cp index);
//     a redefined caller keeps its ClassId, so the entry must retire with the
//     redefinition (InvokeCache::get drops its maps once the redefinition
//     count moves), or `super.hitA()` keeps calling hitA;
//   * the compiled `ldc` slot memo keys a slot on (holder, cp index); the
//     redefined holder's slots must leave the slot table
//     (retire_ldc_slots_of_class), or the recompiled `tag()` keeps answering
//     the old literal.
//
// `Target.tag()` returns the literal "hitA", `Target.viaSuper()` calls
// `super.hitA()` (invokespecial Base.hitA) and `Target.stag()` returns the
// literal too. A transformer rewrites the UTF-8 "hitA" to "hitB" in Target's
// class bytes (same length; Target declares no method of that name, so its
// shape is unchanged): afterwards the same constant-pool indices name the
// literal "hitB" and Base.hitB. Target is warmed first, so every site is cached
// (and, with the JIT on, compiled) before the retransform.
//
// HotSpot 25 prints (with the agent):
//     before=hitA/1/hitA
//     after=hitB/2/hitB
//     again=hitB/2/hitB
// A stale cache prints hitA or 1 after the retransform.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineCallerLdcProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineCallerLdcProbe*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineCallerLdcProbe
// Without the agent (the plain probe runner) it prints before=hitA/1/hitA,
// then "no agent" and after=hitA/1/hitA / again=hitA/1/hitA on both VMs.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class RedefineCallerLdcProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Base {
        public int hitA() {
            return 1;
        }

        public int hitB() {
            return 2;
        }
    }

    public static class Target extends Base {
        public String tag() {
            return "hitA";
        }

        public int viaSuper() {
            return super.hitA();
        }

        public static String stag() {
            return "hitA";
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineCallerLdcProbe$Target".equals(className)) {
                return null;
            }
            byte[] out = bytes.clone();
            byte[] from = {'h', 'i', 't', 'A'};
            for (int i = 0; i + from.length <= out.length; i++) {
                boolean match = true;
                for (int j = 0; j < from.length; j++) {
                    if (out[i + j] != from[j]) {
                        match = false;
                        break;
                    }
                }
                if (match) {
                    out[i + 3] = 'B';
                }
            }
            return out;
        }
    }

    static String once(Target t) {
        return t.tag() + "/" + t.viaSuper() + "/" + Target.stag();
    }

    static String warm(Target t, int n) {
        String last = null;
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += t.tag().length() + t.viaSuper() + Target.stag().length();
            last = once(t);
        }
        return sum == 0 ? "never" : last;
    }

    public static void main(String[] args) throws Exception {
        Target t = new Target();
        System.out.println("before=" + warm(t, 50_000));
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(Target.class);
        } else {
            System.out.println("no agent");
        }
        System.out.println("after=" + once(t));
        System.out.println("again=" + warm(t, 50_000));
    }
}
