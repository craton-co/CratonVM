// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 124, lane L6: the positive control the w122 runs
// of `L4W115VirtualVoidLeafUnmount` and `L9W116RemountedLeafOsrReturn` never
// produced (pages i26-L4-proposal-let-a-virtual-thread-unmount-under-the-
// method-handle-natives, "Wave 124 (lane L6)", and i116-L9-osr-return-skips-
// continuation-return-adapters, "Wave 124 (lane L6)").
//
// Two doors could owe a remounted leaf's caller a return conversion:
//   * the METHOD-HANDLE door (`lang_invoke::transparent_leaf_call`, wave 115
//     `CRATONVM_MH_VOID_TAIL_UNMOUNT`). It serves only this VM's own handle
//     objects (`MH_KIND` slots). Over a real JDK 25 nothing mints them:
//     `Lookup.findStatic` has bytecode, and `resolve_dispatch` step 3 runs it
//     ahead of the `Bridge` in BOTH policies. So `CRATONVM_COMPAT_ALIGN=
//     -jdk-invoke` does not bring them back either (measured on w122: 24
//     `[MH_FOREIGN]`, 0 `[MH_TAIL_ADAPTER]`). A real handle's door is opaque,
//     so the park in `voidLeaf` PINS: no remount, no adapter. The
//     `void-leaf` rows are a stdout control only.
//   * the LAMBDA door (wave 25, `lambda::try_lambda_dispatch_noted`). A
//     `Supplier<Integer>` over the `int` leaf yields and records the boxing
//     adapter (w122 census: `yielded=6`, one per round). The w122 runs still
//     never reached `try_osr_offer`'s adapter arm, because the remounted
//     `intLeaf` frame was an OWNED frame. `try_osr_offer` refuses OSR to a
//     virtual thread's frame with no `cached_method()`. The lambda door's
//     static arm runs the impl through `invoke_static_shared` (by-name
//     `interpreter::execute`, which pushes an owned frame) until
//     `record_lambda_global_impl_owner` has memoised the owner. It memoises
//     only after a call that RETURNED in that Rust frame, and every
//     virtual-thread call yields first, so the memo was never filled.
//     Here one `sup.get()` on the main (platform) thread fills it. The
//     virtual-thread calls then take `try_invoke_cached_lambda_impl`, whose
//     frame is CACHED and may be offered OSR.
//
// Each leaf also runs four times on the main thread with its park off, so its
// loop's OSR body is published (`ir-osr ... (cached)`) before the
// virtual-thread rounds. The remounted activation then enters it at its first
// due back edge instead of racing a background compile.
//
// Run: javac -d . L6W124RemountedLeafAdapters.java
//      java -cp . L6W124RemountedLeafAdapters                      (HotSpot)
//   A. defaults, both modes, JIT on (the lambda door's OSR arm):
//      CRATONVM_DBG_OSR_RETURN_ADAPTER=1 cratonvm --jdk-only -cp . L6W124RemountedLeafAdapters
//      CRATONVM_DBG_OSR_RETURN_ADAPTER=1 cratonvm --compatible -cp . L6W124RemountedLeafAdapters
//      Pass: stdout = the block below. stderr has >= 1
//      `[cratonvm-osr] return-adapter intLeaf()I ... value=some`, each followed
//      by `outcome=converted`, and no `voidLeaf` line.
//   B. the dead MH door, both modes:
//      CRATONVM_MH_VOID_TAIL_UNMOUNT=1 CRATONVM_DBG_MH_DISPATCH=1 cratonvm --jdk-only -cp . L6W124RemountedLeafAdapters
//      Pass: stdout = the block; `grep -c void-leaf` = 0.
//   Each arm also with --nojit: stdout = the block, and no `return-adapter`
//   line (no OSR).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// warm sink 28000000
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

public class L6W124RemountedLeafAdapters {
    static long sink;
    /** Off while the leaves warm up on the platform thread. */
    static volatile boolean park;

    /** Parks (pins under a real handle's door), then a loop. */
    static void voidLeaf(int round) {
        if (park) {
            LockSupport.parkNanos(1_000_000L);
        }
        long s = 0;
        for (int i = 0; i < 2_000_000; i++) {
            s += (i ^ round) & 7;
        }
        sink += s;
    }

    /** Parks (unmounts under the lambda door), then a loop OSR enters after the remount. */
    static int intLeaf() {
        if (park) {
            LockSupport.parkNanos(1_000_000L);
        }
        int s = 0;
        for (int i = 0; i < 2_000_000; i++) {
            s += i & 7;
        }
        return s - 14;
    }

    public static void main(String[] args) throws Throwable {
        Supplier<Integer> sup = L6W124RemountedLeafAdapters::intLeaf;
        // FIRST, before `intLeaf` is compiled: one call that returns, on the
        // platform thread, so the lambda door memoises the impl's owner and
        // later calls run the impl in a cached frame.
        long warmInt = sup.get();
        // Then each leaf's loop is offered OSR here, so its OSR body is
        // published before the virtual-thread rounds.
        for (int k = 0; k < 4; k++) {
            voidLeaf(k);
            warmInt += intLeaf();
        }
        Thread.sleep(300);
        System.out.println("warm sink " + (sink + warmInt - 5L * 6_999_986));
        sink = 0;
        park = true;

        MethodHandle mh = MethodHandles.lookup().findStatic(
                L6W124RemountedLeafAdapters.class,
                "voidLeaf",
                MethodType.methodType(void.class, int.class));
        StringBuilder out = new StringBuilder();
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                for (int round = 0; round < 6; round++) {
                    // A reference call site over a `void` handle.
                    Object r = mh.invoke(round);
                    out.append("void-leaf round ").append(round).append(" -> ").append(r).append('\n');
                    // A lambda proxy over an `int` leaf: the door owes the box.
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
