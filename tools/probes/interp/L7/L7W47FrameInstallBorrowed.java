// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L7: proposal
// docs/internal/fixed-bugs/interpreter-L7-proposal-a-cached-frame-install-without-refcount-or-flag-traffic-RETIRED-20261015.md,
// item 1 (`CRATONVM_FRAME_INSTALL_BORROWED`, `VmConfig::frame_install_borrowed`).
// A fast door whose retired frame slot already runs the callee's cache entry
// rebuilds it without cloning the entry (`invoke_fast::DoorCallee::SlotAlias`,
// `FrameStack::push_cached_compact_reusing_same`). Every shape that reaches the
// install or leaves it early: static and virtual loops calling one callee
// (the aliased install), alternating callees at one depth (the slot runs the
// other entry: a clone), recursion, a synchronized callee, a callee that
// throws (the frame retired by the unwind is reused next), and an instance
// call through the non-virtual door (`private`).
//
// Positive control: `CRATONVM_DBG_FIELD_SITE=1` prints the site-cache tally
// on stderr at exit; with the switch on, its last field `install-same:` is
// well above zero (the loops below make ~10^6 aliased installs), and with
// `CRATONVM_FRAME_INSTALL_BORROWED=0` it is 0 with identical stdout.
//
// Expected (HotSpot 25, `java L7W47FrameInstallBorrowed`):
//   static 1000000
//   virtual 1000000
//   alternate 1500000
//   recursion 500500
//   sync 200000
//   throws 100000
//   private 300000
//   done
public class L7W47FrameInstallBorrowed {
    int k = 1;

    static int one(int x) { return x + 1; }
    static int two(int x) { return x + 2; }

    int inst(int x) { return x + k; }

    private int priv(int x) { return x + 3; }

    static synchronized int locked(int x) { return x + 1; }

    static int thrower(int x) {
        if ((x & 1) == 0) throw new IllegalStateException("even");
        return 1;
    }

    static int sum(int n) { return n == 0 ? 0 : n + sum(n - 1); }

    public static void main(String[] args) {
        int s = 0;
        for (int i = 0; i < 1_000_000; i++) s = one(s) - 0;
        System.out.println("static " + s);

        L7W47FrameInstallBorrowed o = new L7W47FrameInstallBorrowed();
        int v = 0;
        for (int i = 0; i < 1_000_000; i++) v = o.inst(v);
        System.out.println("virtual " + v);

        int a = 0;
        for (int i = 0; i < 1_000_000; i++) a = ((i & 1) == 0) ? one(a) - 0 : two(a) - 1;
        System.out.println("alternate " + (a + 500_000));

        int r = 0;
        for (int i = 0; i < 1_000; i++) r = sum(1_000);
        System.out.println("recursion " + r);

        int y = 0;
        for (int i = 0; i < 200_000; i++) y = locked(y);
        System.out.println("sync " + y);

        int t = 0;
        for (int i = 0; i < 200_000; i++) {
            try {
                t += thrower(i);
            } catch (IllegalStateException e) {
                t += 0;
            }
        }
        System.out.println("throws " + t);

        int p = 0;
        for (int i = 0; i < 100_000; i++) p = o.priv(p);
        System.out.println("private " + p);
        System.out.println("done");
    }
}
