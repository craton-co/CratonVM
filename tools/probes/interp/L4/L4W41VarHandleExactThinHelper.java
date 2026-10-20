// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 41, lane L4
// (`interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010`, the
// thin-helper remainder): a mistyped access on an invoke-exact `VarHandle`
// from a hot loop whose site descriptor has the SAME value kind as the
// handle but another coordinate type (`(Object)int` against `(H)int`).
// `L4W37VarHandleExact`'s `hot-wrong` (`(H)long` against `(H)int`) cannot
// reach a thin helper's fast arm: the helper declines a value kind that is
// not the field's (to the unjudged stand-in cold arm; why that row matched
// on the host is open on the page). These sites are served by the fast arm:
// `varhandle_{read,write}_helper_slot` bind `(Ljava/lang/Object;)I` and
// `(Ljava/lang/Object;I)V` (one reference coordinate, `int` value), the
// handle's plan (`varhandle_instance_field_plan_for_handle`) describes an
// `int` field of `H`, the receiver is an `H`, so the fast arm serves the
// access; and the cold arm's stand-in site is not judged either. Read from
// the code, not measured on CratonVM.
//
//   hot-object-get   `(int) X_EXACT.get((Object) h)` 20 000 times
//   hot-object-set   `X_EXACT.set((Object) h, 3)` 20 000 times
//
// Each row counts the `WrongMethodTypeException`s. Expected on CratonVM
// before the fix of `i37-L4-proposal-thin-varhandle-helpers-know-their-call-site`:
// `--nojit` matches HotSpot; the default (JIT) mode prints less than 20000
// once the loop is compiled with the thin helper bound (to be confirmed on
// the host). `--compatible` mints no exact handle (by design): 0 and 0.
//
// Positive control for the bind: `CRATONVM_DBG_JITC=1` lists the compile of
// `hotGet` / `hotSet` (or their OSR bodies); the per-thread
// `vh_direct_served` counter of the read/write helpers is non-zero.
//
// Run: javac -d out L4W41VarHandleExactThinHelper.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W41VarHandleExactThinHelper
//
// Expected HotSpot 25 output (default and -Xint):
//   hot-object-get: 20000
//   hot-object-set: 20000
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.invoke.WrongMethodTypeException;

public class L4W41VarHandleExactThinHelper {
    static class H {
        int x = 3;
    }

    static final VarHandle X_EXACT;

    static {
        try {
            X_EXACT = MethodHandles.lookup().findVarHandle(H.class, "x", int.class).withInvokeExactBehavior();
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    static int hotGet(H h, int n) {
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            try {
                int v = (int) X_EXACT.get((Object) h);
                if (v != 3) {
                    return -1;
                }
            } catch (WrongMethodTypeException e) {
                thrown++;
            }
        }
        return thrown;
    }

    static int hotSet(H h, int n) {
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            try {
                X_EXACT.set((Object) h, 3);
            } catch (WrongMethodTypeException e) {
                thrown++;
            }
        }
        return thrown;
    }

    public static void main(String[] args) {
        H h = new H();
        System.out.println("hot-object-get: " + hotGet(h, 20_000));
        System.out.println("hot-object-set: " + hotSet(h, 20_000));
    }
}
