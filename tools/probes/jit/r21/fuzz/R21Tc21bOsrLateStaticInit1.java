// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 tc21b (TYPECHECK21-8 of jit-r21-typecheck-proposals-20261014):
// the optimizing OSR memo and a static of a class that initializes AFTER the
// loop body was built. `drive(n, false)` runs a long loop whose only `Holder`
// access sits behind a flag that is false, so the OSR body is built while
// `Holder` is uninitialized (its `getstatic` / `putstatic` are helper calls).
// The following `drive(n, true)` calls initialize `Holder` on their first
// trip. With `CRATONVM_JIT_OSR_REBUILD_STATIC_INIT=1` the memo entry is
// forgotten once `Holder` is initialized and the next OSR entry rebuilds the
// body with direct static accesses; with it unset the helper form is kept.
// The answers must be identical on every arm; only the "time" rows move.
//
// Expected output is DERIVED from JLS 12.4 / 15.26.2, NOT measured (no JDK 25
// HotSpot on the authoring host). With N = 2_000_000 (a multiple of 8):
//   * drive(N, false) = sum of (i & 7) = (N / 8) * 28 = 7_000_000;
//   * drive(N, true): Holder.v starts at 3 and toggles 3, 2, 3, 2, ... (v ^= 1)
//     once per iteration, so each call adds (N/2)*3 + (N/2)*2 = 5_000_000 on
//     top of the 7_000_000, and leaves v at 3 again (N is even):
//     12_000_000 on every `true` call.
//
// Run:
//   javac -d out R21Tc21bOsrLateStaticInit1.java
//   java -Xmx256m -cp out R21Tc21bOsrLateStaticInit1                   # HotSpot 25 (when available)
//   cratonvm -Xmx256m -cp out R21Tc21bOsrLateStaticInit1
//   cratonvm --compatible -Xmx256m -cp out R21Tc21bOsrLateStaticInit1
//   CRATONVM_JIT_OSR_REBUILD_STATIC_INIT=1 cratonvm -Xmx256m -cp out R21Tc21bOsrLateStaticInit1
//   CRATONVM_JIT_OSR_REBUILD_STATIC_INIT=1 CRATONVM_DBG_JITC=1 cratonvm ... 2>&1 | grep "FORGETS a body built before a class initialized"
// PASS: every "fact" line reads OK on every arm in both modes. With the switch
// on and CRATONVM_DBG_JITC=1 the FORGETS line appears once; the "time" rows of
// calls 2.. are then expected lower than with the switch unset (a decision
// row for the opt-in switch, not a fact).

public class R21Tc21bOsrLateStaticInit1 {
    static final int N = 2_000_000;
    static final int CALLS = 6;

    static final class Holder {
        static int v;
        static {
            v = 3;
        }
    }

    static long drive(int n, boolean use) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i & 7;
            if (use) {
                s += Holder.v;
                Holder.v ^= 1;
            }
        }
        return s;
    }

    public static void main(String[] args) {
        boolean ok = true;
        for (int c = 0; c < CALLS; c++) {
            boolean use = c > 0;
            long t0 = System.nanoTime();
            long got = drive(N, use);
            long dt = System.nanoTime() - t0;
            long want = use ? 12_000_000L : 7_000_000L;
            boolean good = got == want;
            ok &= good;
            System.out.println("fact call" + c + " " + got + (good ? " OK" : " FAIL want " + want));
            System.out.println("time call" + c + " ns/iter " + (dt / N));
        }
        System.out.println(ok ? "ALL OK" : "FAILED");
    }
}
