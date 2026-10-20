// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i5 wave 106, lane L2 (page
// docs/internal/fixed-bugs/interpreter-i101-L2-pc-unknown-handler-search-matches-typed-rows-outside-their-range-FIXED-20261016.md,
// "Wave 106"). Three shapes whose throwable reaches a compiled caller as a VM
// re-post (`handle_jit_dispatch_error`) with no throw-site stamp:
//
//   rethrow   L1W93XPair's shape: a catch that throws a new exception, the
//             w105 battery's 115,360 `JITC-UNSTAMPED-ESCAPE` lines;
//   delegate  an `Op.run(I)I` whose catch covers its call to ANOTHER
//             `Op.run(I)I` (same name and descriptor) and whose own throw is
//             outside the try: an escape record of the inner run must not make
//             the outer run's caller skip, or take, the outer table;
//   clinit    a class-initialization failure at an `invokestatic` outside the
//             try of a method that catches the same error types elsewhere.
//
// HotSpot runs no handler for an uncovered throw. Every count is exact.
//
// Run (JIT on, both modes; also with CRATONVM_DBG_JITC=1 and count lines
// tagged JITC-UNSTAMPED-ESCAPE and JITC-PC-CENSUS: want 0 of each; and with
// CRATONVM_JIT_DISPATCH_ERROR_ESCAPE_RECORD=0 for the old route, same stdout):
//   javac -d out L2W106RethrowEscapeShapes.java
//   cratonvm -cp out L2W106RethrowEscapeShapes
//
// HOTSPOT_EXPECTED_BEGIN
// rethrow uoe=60000 cause=60000 other=0
// delegate ok=40000 outer=20000 inner=0
// clinit handled=0 eiie=1 ncdfe=59999 other=0
// HOTSPOT_EXPECTED_END
public class L2W106RethrowEscapeShapes {
    static int sink;

    // ---- rethrow ---------------------------------------------------------
    static int callee(int x) {
        if (x >= 0) {
            throw new IllegalStateException("x");
        }
        return x;
    }

    static int rethrow(int x) {
        try {
            return callee(x);
        } catch (IllegalStateException e) {
            throw new UnsupportedOperationException(e);
        }
    }

    // ---- delegate --------------------------------------------------------
    interface Op {
        int run(int x);
    }

    static final class Inner implements Op {
        public int run(int x) {
            if ((x & 1) == 0) {
                throw new IllegalArgumentException("inner");
            }
            return x;
        }
    }

    static final class Outer implements Op {
        final Op next;

        Outer(Op next) {
            this.next = next;
        }

        public int run(int x) {
            int r;
            try {
                r = next.run(x);
            } catch (IllegalArgumentException e) {
                r = -1;
            }
            if (x % 3 == 0) {
                throw new IllegalArgumentException("outer");
            }
            return r;
        }
    }

    // ---- clinit ----------------------------------------------------------
    static int handled;

    static int zero() {
        return 0;
    }

    static class Bad {
        static int v = 1 / zero();

        static int get() {
            return v;
        }
    }

    static int safe(int k) {
        return k & 7;
    }

    static int initSite(int k) {
        int r;
        try {
            r = safe(k);
        } catch (ExceptionInInitializerError | NoClassDefFoundError e) {
            handled++;
            r = -100;
        }
        r += Bad.get();
        return r;
    }

    public static void main(String[] args) {
        int uoe = 0, cause = 0, other = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 20_000; i++) {
                try {
                    sink += rethrow(i);
                    other++;
                } catch (UnsupportedOperationException e) {
                    uoe++;
                    if (e.getCause() instanceof IllegalStateException) {
                        cause++;
                    }
                } catch (RuntimeException e) {
                    other++;
                }
            }
        }
        System.out.println("rethrow uoe=" + uoe + " cause=" + cause + " other=" + other);

        Op op = new Outer(new Inner());
        int ok = 0, outer = 0, inner = 0;
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 20_000; i++) {
                int x = round * 20_000 + i;
                try {
                    sink += op.run(x);
                    ok++;
                } catch (IllegalArgumentException e) {
                    if ("outer".equals(e.getMessage())) {
                        outer++;
                    } else {
                        inner++;
                    }
                }
            }
        }
        System.out.println("delegate ok=" + ok + " outer=" + outer + " inner=" + inner);

        int eiie = 0, ncdfe = 0, bad = 0;
        for (int i = 0; i < 60_000; i++) {
            try {
                sink += initSite(i);
                bad++;
            } catch (ExceptionInInitializerError e) {
                eiie++;
            } catch (NoClassDefFoundError e) {
                ncdfe++;
            } catch (Throwable t) {
                bad++;
            }
        }
        System.out.println("clinit handled=" + handled + " eiie=" + eiie + " ncdfe=" + ncdfe
                + " other=" + bad);
    }
}
