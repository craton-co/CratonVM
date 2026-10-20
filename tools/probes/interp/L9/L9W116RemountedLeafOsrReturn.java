// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 116, lane L9 (review; page
// `interpreter-i116-L9-osr-return-skips-continuation-return-adapters-FIXED-20261224`): a
// virtual thread's leaf that unmounted under a door which owes it a return
// conversion -- the `null` of a `void` leaf behind a method-handle door whose
// caller receives a reference (`CRATONVM_MH_VOID_TAIL_UNMOUNT`), the boxing of
// an `int` leaf behind a `Supplier<Integer>` lambda door (wave 25) -- and then,
// remounted, runs a loop hot enough to be entered through OSR and returns from
// the compiled body. `try_osr_offer`'s return arm pushed the compiled body's
// raw value (or nothing) into the caller, skipping the adapter the
// interpreter's `return` arms pay. Every line must be HotSpot's, both modes,
// JIT and --nojit, with and without the switch.
//
// Run: javac -d . L9W116RemountedLeafOsrReturn.java
//      java -cp . L9W116RemountedLeafOsrReturn                              (HotSpot reference)
//      cratonvm --jdk-only -cp . L9W116RemountedLeafOsrReturn
//      CRATONVM_MH_VOID_TAIL_UNMOUNT=1 cratonvm --jdk-only -cp . L9W116RemountedLeafOsrReturn
//      (and the same two with --compatible, and with --nojit; add
//      CRATONVM_DBG_MH_DISPATCH=1 to see the `[MH_TAIL_ADAPTER] ... void-leaf`
//      records, the positive control that the door was transparent)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// void-leaf round 0 -> null
// int-leaf round 0 -> 6999986 true
// void-leaf round 1 -> null
// int-leaf round 1 -> 6999986 true
// void-leaf round 2 -> null
// int-leaf round 2 -> 6999986 true
// void-leaf round 3 -> null
// int-leaf round 3 -> 6999986 true
// void-leaf round 4 -> null
// int-leaf round 4 -> 6999986 true
// void-leaf round 5 -> null
// int-leaf round 5 -> 6999986 true
// sink 42000000
// HOTSPOT_EXPECTED_END

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.concurrent.locks.LockSupport;
import java.util.function.Supplier;

public class L9W116RemountedLeafOsrReturn {
    static long sink;

    /** Unmounts, then a loop long enough to be offered OSR after the remount. */
    static void voidLeaf(int round) {
        LockSupport.parkNanos(1_000_000L);
        long s = 0;
        for (int i = 0; i < 2_000_000; i++) {
            s += (i ^ round) & 7;
        }
        sink += s;
    }

    /** The same shape returning an `int` that its `Supplier<Integer>` boxes. */
    static int intLeaf() {
        LockSupport.parkNanos(1_000_000L);
        int s = 0;
        for (int i = 0; i < 2_000_000; i++) {
            s += i & 7;
        }
        return s - 14;
    }

    public static void main(String[] args) throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(
                L9W116RemountedLeafOsrReturn.class,
                "voidLeaf",
                MethodType.methodType(void.class, int.class));
        Supplier<Integer> sup = L9W116RemountedLeafOsrReturn::intLeaf;
        StringBuilder out = new StringBuilder();
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                for (int round = 0; round < 6; round++) {
                    // A reference call site over a `void` handle: the door owes `null`.
                    Object r = mh.invoke(round);
                    out.append("void-leaf round ").append(round).append(" -> ").append(r).append('\n');
                    Object v = sup.get();
                    out.append("int-leaf round ").append(round).append(" -> ").append(v)
                            .append(' ').append(v instanceof Integer).append('\n');
                }
            } catch (Throwable e) {
                out.append("threw ").append(e).append('\n');
            }
        });
        t.join();
        System.out.print(out);
        System.out.println("sink " + sink);
    }
}
