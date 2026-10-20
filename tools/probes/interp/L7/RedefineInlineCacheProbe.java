// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 21, lane L1: after a retransform, a compiled
// caller that reaches the retransformed method through an inline cache (a
// virtual call site, not a baked direct call) must run the NEW body on its
// next call. `Caller.call` dispatches `Base.work` on two receiver classes, so
// its site is a MIC / PIC and not a devirtualised inline; `ImplA.work` reads
// an array, so no tier splices it. The cache is filled while `ImplA.work`'s
// first compiled body is current; a later tier-up supersedes that body, and
// before wave 21 a scoped redefinition cleared only cache slots holding the
// CURRENT body, so the slot kept calling the superseded one: the old bytecode
// (docs/internal/fixed-bugs/interpreter-L0-a-running-compiled-caller-keeps-calling-a-redefined-callees-old-body-FIXED-20260925.md).
// Whether a tier-up lands after the fill depends on the tiering; the output
// must be HotSpot's in every case.
//
// A transformer renames "QqA" to "QqB" in `ImplA`'s class bytes (same
// length): its call to `Helper.mQqA()` (returns 1) becomes a call to
// `Helper.mQqB()` (returns 2).
//
// HotSpot 25 prints (with the agent):
//     warm=350000
//     before=1
//     after=2
//     again=2
// Run with and without --nojit; the output must not change.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineInlineCacheProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineInlineCacheProbe*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineInlineCacheProbe
// Without the agent (the plain probe runner) both VMs print warm= and
// before= as above, then "no agent", after=1 and again=1.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class RedefineInlineCacheProbe {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Helper {
        public static int mQqA() {
            return 1;
        }

        public static int mQqB() {
            return 2;
        }
    }

    public abstract static class Base {
        abstract int work(int n);
    }

    public static class ImplA extends Base {
        static final int[] DATA = {0, 0, 0, 0};

        @Override
        int work(int n) {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s += DATA[i & 3];
            }
            return s + Helper.mQqA();
        }
    }

    public static class ImplB extends Base {
        @Override
        int work(int n) {
            return 7;
        }
    }

    public static class Caller {
        static int call(Base b, int n) {
            return b.work(n);
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineInlineCacheProbe$ImplA".equals(className)) {
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

    static int warm(Base a, Base b, int rounds) {
        int s = 0;
        for (int i = 0; i < rounds; i++) {
            s += Caller.call((i & 7) == 0 ? b : a, 3);
        }
        return s;
    }

    public static void main(String[] args) throws Exception {
        Base a = new ImplA();
        Base b = new ImplB();
        System.out.println("warm=" + warm(a, b, 200_000));
        System.out.println("before=" + Caller.call(a, 3));
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(ImplA.class);
        } else {
            System.out.println("no agent");
        }
        System.out.println("after=" + Caller.call(a, 3));
        warm(a, b, 20_000);
        System.out.println("again=" + Caller.call(a, 3));
    }
}
