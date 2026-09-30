// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// gce e2/f (2026-09-29): item 1 of
// docs/known-issues/gc/gengc-r4w5-oomjit5-exceptional-frame-orphans-outlive-a-compiled-catch-20260924.md
// -- the compiled LOCAL-HANDLER commit. A compiled `catcher` calls a compiled
// `callee` directly; `callee` throws an IllegalStateException from inside its
// `try`, whose only handler (ArithmeticException) reads a 16 MiB local, so a
// compiled `callee` publishes its precise exceptional frame naming the array.
// `catcher` catches the exception in its OWN compiled handler. Nothing reads
// the callee's frame after that, so a weak reference to the array must clear
// at the next collections. The page's claim: the local-handler commit
// (`jit_local_handler_lookup`) never looks at the exceptional stash, so the
// frame stays stashed and keeps the array alive (census label
// `vm/deopt-stash`). The opt-in drop is `CRATONVM_JIT_LOCAL_HANDLER_DROP_ORPHANS=1`.
//
// Item 3 (a dispatch-resumed caller) is the second row: the same throw
// reached through an interface call (`Thrower`), which a compiled caller
// makes through its inline cache / the dispatch helper.
//
//   javac -d tools/probes tools/probes/GceE2fLocalCatchOrphanFrameProbe.java
//   java|cratonvm [-XX:+UseG1GC|-XX:+UseZGC] -Xmx64m -cp tools/probes GceE2fLocalCatchOrphanFrameProbe
//
// Expected (HotSpot 25 -XX:+UseSerialGC, and -Xint):
//   PASS local-catch-direct
//   PASS local-catch-interface
//   PASS all 2
// The optional first argument is the warm-up count (default 200000).
import java.lang.ref.WeakReference;

public class GceE2fLocalCatchOrphanFrameProbe {
    static final int BIG = 16 << 20;
    static final byte[] SMALL = new byte[3];
    static WeakReference<byte[]> ref;

    interface Thrower {
        int run(boolean trigger);
    }

    static int callee(boolean trigger) {
        byte[] big = trigger ? new byte[BIG] : SMALL;
        if (trigger) {
            ref = new WeakReference<>(big);
        }
        try {
            if (trigger) {
                throw new IllegalStateException("escapes callee");
            }
            return big.length & 1;
        } catch (ArithmeticException e) {
            return big.length;
        }
    }

    static final class Impl implements Thrower {
        @Override
        public int run(boolean trigger) {
            byte[] big = trigger ? new byte[BIG] : SMALL;
            if (trigger) {
                ref = new WeakReference<>(big);
            }
            try {
                if (trigger) {
                    throw new IllegalStateException("escapes run");
                }
                return big.length & 1;
            } catch (ArithmeticException e) {
                return big.length;
            }
        }
    }

    static final Thrower THROWER = new Impl();

    static int catcherDirect(boolean trigger) {
        try {
            return callee(trigger);
        } catch (IllegalStateException e) {
            return -1;
        }
    }

    static int catcherInterface(Thrower t, boolean trigger) {
        try {
            return t.run(trigger);
        } catch (IllegalStateException e) {
            return -1;
        }
    }

    static boolean cleared() {
        for (int round = 0; round < 5; round++) {
            System.gc();
            if (ref != null && ref.get() == null) {
                return true;
            }
        }
        return false;
    }

    public static void main(String[] args) {
        int warm = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        long sink = 0;
        for (int i = 0; i < warm; i++) {
            sink += catcherDirect(false);
            sink += catcherInterface(THROWER, false);
        }
        int failures = 0;
        ref = null;
        boolean ok = catcherDirect(true) == -1 && cleared();
        failures += ok ? 0 : 1;
        System.out.println((ok ? "PASS" : "FAIL") + " local-catch-direct");
        ref = null;
        ok = catcherInterface(THROWER, true) == -1 && cleared();
        failures += ok ? 0 : 1;
        System.out.println((ok ? "PASS" : "FAIL") + " local-catch-interface");
        System.out.println(failures == 0 ? "PASS all 2" : "FAIL " + failures + " of 2");
        if (failures != 0 || sink < 0) {
            System.exit(1);
        }
    }
}
