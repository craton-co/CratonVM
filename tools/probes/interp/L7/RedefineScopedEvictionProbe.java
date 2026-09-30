// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 20, lane L1: a retransform withdraws the compiled
// code that depends on the retransformed class, and only that code
// (docs/known-issues/interpreter/i14-L3-proposal-class-scoped-redefinition-invalidation-20260925.md,
// "Progress (wave 20)", stage 2b). After the retransform every hot caller must
// run the NEW callee body on its very next call, whichever way it reached the
// callee when it was compiled:
//
//   * inlined: `Caller.inlined` calls the tiny static `Small.value()`;
//   * devirtualised: `Caller.virt` calls `Base.v()` on a receiver that is
//     always `Impl` (a guarded inline, or a class-hierarchy bind);
//   * direct: `Caller.direct` calls `Big.value()`, too large (a loop) to be
//     inlined, so a compiled caller binds a direct call to its body.
//
// A transformer renames "QqA" to "QqB" in the class bytes of Small, Impl and
// Big (same length, so the class file stays well formed): their calls to
// `Helper.mQqA()` (returns 1) become calls to `Helper.mQqB()` (returns 2).
// `Other.work` depends on none of them and must keep its result.
//
// HotSpot 25 prints (with the agent):
//     before=1,1,1,other=499500
//     after=2,2,2,other=499500
//     again=2,2,2,other=499500
// A compiled caller that kept a withdrawn dependency prints a 1 after the
// retransform. Run with and without --nojit; the output must not change.
//
// SETUP: needs a java agent. Build a jar whose manifest has
//     Premain-Class: RedefineScopedEvictionProbe$Agent
//     Can-Retransform-Classes: true
// containing RedefineScopedEvictionProbe*.class, then run
//     java|cratonvm --compatible [--nojit] -javaagent:probe.jar -cp probe.jar RedefineScopedEvictionProbe
// Without the agent (the plain probe runner) both VMs print before= as above,
// then "no agent", and after= / again= with the same values as before=.
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class RedefineScopedEvictionProbe {
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

    public static class Small {
        public static int value() {
            return Helper.mQqA();
        }
    }

    public static class Base {
        public int v() {
            return 0;
        }
    }

    public static class Impl extends Base {
        @Override
        public int v() {
            return Helper.mQqA();
        }
    }

    public static class Big {
        public static int value() {
            int acc = 0;
            for (int i = 0; i < 3; i++) {
                acc += i;
            }
            acc -= 3;
            return acc + Helper.mQqA();
        }
    }

    public static class Caller {
        static int inlined(int n) {
            int last = 0;
            for (int i = 0; i < n; i++) {
                last = Small.value();
            }
            return last;
        }

        static int virt(Base b, int n) {
            int last = 0;
            for (int i = 0; i < n; i++) {
                last = b.v();
            }
            return last;
        }

        static int direct(int n) {
            int last = 0;
            for (int i = 0; i < n; i++) {
                last = Big.value();
            }
            return last;
        }
    }

    public static class Other {
        static int work(int n) {
            int s = 0;
            for (int i = 0; i < n; i++) {
                s += i;
            }
            return s;
        }
    }

    static final class Rename implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (!"RedefineScopedEvictionProbe$Small".equals(className)
                    && !"RedefineScopedEvictionProbe$Impl".equals(className)
                    && !"RedefineScopedEvictionProbe$Big".equals(className)) {
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

    static String round(Base b, int n) {
        int other = 0;
        for (int r = 0; r < 20; r++) {
            other = Other.work(1000);
        }
        return Caller.inlined(n) + "," + Caller.virt(b, n) + "," + Caller.direct(n)
                + ",other=" + other;
    }

    public static void main(String[] args) throws Exception {
        Base b = new Impl();
        String before = "";
        for (int w = 0; w < 20; w++) {
            before = round(b, 5_000);
        }
        System.out.println("before=" + before);
        Instrumentation i = inst;
        if (i != null && i.isRetransformClassesSupported()) {
            i.addTransformer(new Rename(), true);
            i.retransformClasses(Small.class, Impl.class, Big.class);
        } else {
            System.out.println("no agent");
        }
        System.out.println("after=" + round(b, 1));
        String again = "";
        for (int w = 0; w < 20; w++) {
            again = round(b, 5_000);
        }
        System.out.println("again=" + again);
    }
}
