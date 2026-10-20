// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe -- Interpreter round i5 wave 106, lane L2 (review of the 2026-10-14
// netty-compression-wall JIT passes; page
// docs/internal/fixed-bugs/interpreter-i106-L2-netty-wall-jit-passes-review-REVIEWED-20261016.md).
// Shapes the optimizing tier now splices into a caller whose call sits in a
// `try`, so the callee's `athrow` may become a compiled edge into the caller's
// handler (`CRATONVM_JIT_IR_SPLICED_THROW_EDGES`) or a guard in front of a
// masked cold arm (`CRATONVM_JIT_IR_COLD_THROW_TAIL_ARMS`):
//
//   tail     `if (o != null) return o; throw new ISE` -- a TAIL arm entered
//            only by the `ifnull`'s taken edge;
//   foreign  a `static final` preallocated IllegalArgumentException thrown
//            under a `catch (IllegalStateException)` row first and a
//            `catch (IllegalArgumentException)` row second: the edge tests
//            only the FIRST covering row, so it must deopt, not branch;
//   null     `throw NULLEXC` where the static is null: an NPE with HotSpot's
//            helpful message, never a branch into the handler with null.
//
// Every count is exact; the loops are long enough for OSR and method-entry
// compiles of every method.
//
// Run (JIT on, both modes; also with CRATONVM_JIT_IR_SPLICED_THROW_EDGES=0
// and CRATONVM_JIT_IR_COLD_THROW_TAIL_ARMS=0, same stdout):
//   javac -d out L2W106SplicedThrowEdgeShapes.java
//   cratonvm -cp out L2W106SplicedThrowEdgeShapes
//
// HOTSPOT_EXPECTED_BEGIN
// tail ise=8572 ok=51428
// foreign ise=0 iae=3750 ok=56250
// null npe=1875 ok=58125 other=0
// null message=Cannot throw exception because "L2W106SplicedThrowEdgeShapes.NULLEXC" is null
// HOTSPOT_EXPECTED_END
public class L2W106SplicedThrowEdgeShapes {
    static int sink;
    static final RuntimeException PRE = new IllegalArgumentException("pre");
    static RuntimeException NULLEXC = null;
    static String npeMessage;

    static Object orThrow(Object o) {
        if (o != null) {
            return o;
        }
        throw new IllegalStateException("null");
    }

    static int pick(int i) {
        if ((i & 15) == 0) {
            throw PRE;
        }
        return i;
    }

    static int nul(int i) {
        if ((i & 31) == 0) {
            throw NULLEXC;
        }
        return i;
    }

    static void tail() {
        int ise = 0, ok = 0;
        for (int i = 0; i < 60_000; i++) {
            try {
                sink += orThrow(i % 7 == 0 ? null : "s").hashCode() & 1;
                ok++;
            } catch (IllegalStateException e) {
                ise++;
            }
        }
        System.out.println("tail ise=" + ise + " ok=" + ok);
    }

    static void foreign() {
        int ise = 0, iae = 0, ok = 0;
        for (int i = 0; i < 60_000; i++) {
            try {
                sink += pick(i);
                ok++;
            } catch (IllegalStateException e) {
                ise++;
            } catch (IllegalArgumentException e) {
                iae++;
            }
        }
        System.out.println("foreign ise=" + ise + " iae=" + iae + " ok=" + ok);
    }

    static void nulls() {
        int npe = 0, ok = 0, other = 0;
        for (int i = 0; i < 60_000; i++) {
            try {
                sink += nul(i);
                ok++;
            } catch (NullPointerException e) {
                npe++;
                if (npeMessage == null) {
                    npeMessage = e.getMessage();
                }
            } catch (RuntimeException e) {
                other++;
            }
        }
        System.out.println("null npe=" + npe + " ok=" + ok + " other=" + other);
        System.out.println("null message=" + npeMessage);
    }

    public static void main(String[] args) {
        tail();
        foreign();
        nulls();
    }
}
