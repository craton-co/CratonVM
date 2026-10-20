// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L4 (`CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH`,
// default off; proposal
// docs/internal/proposals/interpreter/i100-L4-proposal-method-handle-warm-call-directions-20261202.md,
// item 3): a warm real `DirectMethodHandle` of a method is served before the
// JDK linker while the VM's "hidden frames never observed" latch holds, and
// the skipped linker's `checkCustomized` count is kept, so the handle
// customizes after 127 calls exactly as on the linker path (HotSpot then shows
// a spun `LambdaForm$DMH` frame for it).
//
// Rows: answers and default walks / traces (hidden frames filtered) during the
// latched phase; then the FIRST `SHOW_HIDDEN_FRAMES` walk (the one that ends
// the latch: it runs inside a call the lane may have served, so its holder
// frames come from the stand-in table, row `first-hidden`, a known residual
// under the switch); then later hidden walks, which must be HotSpot's in every
// arm (`second-hidden`, `fresh-handle-hidden`, `fresh-after-127-hidden`).
//
// Run: javac -d . L4W117DirectHiddenLatch.java
//      java -cp . L4W117DirectHiddenLatch                                   (HotSpot)
//      cratonvm --jdk-only|--compatible [--nojit] -cp . L4W117DirectHiddenLatch
//      CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH=1 cratonvm ... L4W117DirectHiddenLatch
//      Bench: L4W102DirectMethodWarmBench with CRATONVM_MH_REAL_DIRECT_HIDDEN_LATCH=1
//      against both CRATONVM_MH_REAL_DIRECT_BEFORE_LINKER=1 and neither.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// warm-sum 45150
// default-walk 42 [L4W117DirectHiddenLatch::target, L4W117DirectHiddenLatch::main]
// thrown t5 [L4W117DirectHiddenLatch::thrower]
// first-hidden [L4W117DirectHiddenLatch::target, java.lang.invoke.LambdaForm$DMH::invokeStatic, java.lang.invoke.LambdaForm$MH::invokeExact_MT]
// second-hidden [L4W117DirectHiddenLatch::target, java.lang.invoke.LambdaForm$DMH::invokeStatic, java.lang.invoke.LambdaForm$MH::invokeExact_MT]
// fresh-handle-hidden [L4W117DirectHiddenLatch::target, java.lang.invoke.DirectMethodHandle$Holder::invokeStatic, java.lang.invoke.LambdaForm$MH::invokeExact_MT]
// fresh-warm-sum 20100
// fresh-after-127-hidden [L4W117DirectHiddenLatch::target, java.lang.invoke.LambdaForm$DMH::invokeStatic, java.lang.invoke.LambdaForm$MH::invokeExact_MT]
// HOTSPOT_EXPECTED_END
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;

public class L4W117DirectHiddenLatch {
    static final StackWalker DEFAULT = StackWalker.getInstance();
    static boolean hidden;
    static List<String> last;

    static String fold(String s) {
        return s.replaceAll("([/.])0x[0-9a-f]+", "$1ADDR")
                .replaceAll("LambdaForm\\$(\\w+)/ADDR", "LambdaForm\\$$1");
    }

    static List<String> walk(StackWalker w) {
        return w.walk(s -> {
            List<String> out = new ArrayList<>();
            for (StackFrame f : (Iterable<StackFrame>) s::iterator) {
                if (f.getClassName().equals(L4W117DirectHiddenLatch.class.getName())
                        && f.getMethodName().startsWith("phase")) {
                    break;
                }
                if (!f.getMethodName().equals("walk")) {
                    out.add(fold(f.getClassName() + "::" + f.getMethodName()));
                }
            }
            return out;
        });
    }

    static int target(int x) {
        if (hidden) {
            last = walk(StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES));
        } else if (x == 41) {
            last = walk(DEFAULT);
        }
        return x + 1;
    }

    static int thrower(int x) {
        throw new IllegalStateException("t" + x);
    }

    static int phaseCalls(MethodHandle h, int n) throws Throwable {
        int sum = 0;
        for (int i = 0; i < n; i++) {
            sum += (int) h.invokeExact(i);
        }
        return sum;
    }

    static List<String> phaseHidden(MethodHandle h) throws Throwable {
        hidden = true;
        int r = (int) h.invokeExact(7);
        hidden = false;
        return last;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType it = MethodType.methodType(int.class, int.class);
        MethodHandle h = l.findStatic(L4W117DirectHiddenLatch.class, "target", it);
        MethodHandle t = l.findStatic(L4W117DirectHiddenLatch.class, "thrower", it);

        System.out.println("warm-sum " + phaseCalls(h, 300));
        last = null;
        int r = (int) h.invokeExact(41);
        System.out.println("default-walk " + r + " " + last);
        try {
            int x = (int) t.invokeExact(5);
            System.out.println("thrown none " + x);
        } catch (IllegalStateException e) {
            List<String> tr = new ArrayList<>();
            for (StackTraceElement el : e.getStackTrace()) {
                if (el.getMethodName().startsWith("main")) {
                    break;
                }
                tr.add(fold(el.getClassName() + "::" + el.getMethodName()));
            }
            System.out.println("thrown " + e.getMessage() + " " + tr);
        }
        System.out.println("first-hidden " + phaseHidden(h));
        System.out.println("second-hidden " + phaseHidden(h));
        MethodHandle fresh = l.findStatic(L4W117DirectHiddenLatch.class, "target", it);
        System.out.println("fresh-handle-hidden " + phaseHidden(fresh));
        System.out.println("fresh-warm-sum " + phaseCalls(fresh, 200));
        System.out.println("fresh-after-127-hidden " + phaseHidden(fresh));
    }
}
