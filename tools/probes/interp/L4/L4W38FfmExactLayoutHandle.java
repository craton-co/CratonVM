// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4: an FFM layout `VarHandle` made
// invoke-exact (`ValueLayout.varHandle().withInvokeExactBehavior()`) refuses a
// mistyped access as HotSpot does
// (docs/internal/fixed-bugs/interpreter-L4-varhandle-views-exact-behavior-and-access-mode-queries-FIXED-20261010.md,
// Progress (wave 37), "a mistyped access to an explicitly exact FFM layout
// handle converts").
//
// Before wave 38 `varhandle_exact_call_site_refusal` declined every layout
// handle, so each `*-wmte` row converted and printed a value; and an exact
// layout handle alone never opened the VM's exact gate (only a field or
// array handle's mint did). The coordinates of a layout handle are
// `(MemorySegment, long)`, plus one `long` per open sequence index.
//
// `cas-ok` / `plain-long-return` are the read-modify-write modes of a layout
// handle, which only knew `get` and `set` until the wave-38 follow-up
// (`lang_invoke::layout_vh_rmw`): the wave-38 host run printed `false 9` /
// `9` in every mode.
//
// Positive control: CRATONVM_DBG_MH_STACK=1 prints
// `[MH_STACK] exact VarHandle get(Ljava/lang/foreign/MemorySegment;I)I: refused`
// for the `get-int-offset-wmte` row.
//
// Run: javac -d out L4W38FfmExactLayoutHandle.java
//      cratonvm --java-home <jdk25> [--nojit] -cp out L4W38FfmExactLayoutHandle
//
// `--compatible` answers `withInvokeExactBehavior()` with the handle itself
// (by design, wave 37): its `flag` row prints `false` and every `*-wmte` row
// converts.
//
// Expected HotSpot 25 output (default and -Xint):
//   flag: true
//   get-ok: 7
//   get-int-offset-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (MemorySegment,long)int but found (MemorySegment,int)int
//   get-long-return-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (MemorySegment,long)int but found (MemorySegment,long)long
//   set-ok: 9
//   set-short-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (MemorySegment,long,int)void but found (MemorySegment,long,short)void
//   cas-ok: true 11
//   plain-long-return: 11
//   seq-ok: 3
//   seq-int-index-wmte: java.lang.invoke.WrongMethodTypeException: handle's method type (MemorySegment,long,long)int but found (MemorySegment,long,int)int
//   hot-wrong: 20000
import java.lang.foreign.MemoryLayout;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.VarHandle;

public class L4W38FfmExactLayoutHandle {
    interface Body {
        String run() throws Throwable;
    }

    static void row(String label, Body body) {
        String out;
        try {
            out = body.run();
        } catch (Throwable t) {
            out = t.getMessage() == null ? t.getClass().getName() : t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(label + ": " + out);
    }

    static int wrongOnce(VarHandle vh, MemorySegment seg) {
        try {
            long v = (long) vh.get(seg, 0L);
            return v == 11 ? 0 : 0;
        } catch (java.lang.invoke.WrongMethodTypeException e) {
            return 1;
        }
    }

    public static void main(String[] args) {
        MemorySegment seg = MemorySegment.ofArray(new int[] {7, 0, 3, 0});
        VarHandle plain = ValueLayout.JAVA_INT.varHandle();
        VarHandle exact = plain.withInvokeExactBehavior();
        row("flag", () -> String.valueOf(exact.hasInvokeExactBehavior()));
        row("get-ok", () -> String.valueOf((int) exact.get(seg, 0L)));
        row("get-int-offset-wmte", () -> String.valueOf((int) exact.get(seg, 0)));
        row("get-long-return-wmte", () -> String.valueOf((long) exact.get(seg, 0L)));
        row("set-ok", () -> {
            exact.set(seg, 0L, 9);
            return String.valueOf((int) plain.get(seg, 0L));
        });
        row("set-short-wmte", () -> {
            exact.set(seg, 0L, (short) 5);
            return "stored " + (int) plain.get(seg, 0L);
        });
        row("cas-ok", () -> {
            boolean swapped = (boolean) exact.compareAndSet(seg, 0L, 9, 11);
            return swapped + " " + (int) plain.get(seg, 0L);
        });
        row("plain-long-return", () -> String.valueOf((long) plain.get(seg, 0L)));
        VarHandle seq = MemoryLayout.sequenceLayout(4, ValueLayout.JAVA_INT)
                .varHandle(MemoryLayout.PathElement.sequenceElement()).withInvokeExactBehavior();
        row("seq-ok", () -> String.valueOf((int) seq.get(seg, 0L, 2L)));
        row("seq-int-index-wmte", () -> String.valueOf((int) seq.get(seg, 0L, 2)));
        row("hot-wrong", () -> {
            int refused = 0;
            for (int i = 0; i < 20_000; i++) {
                refused += wrongOnce(exact, seg);
            }
            return String.valueOf(refused);
        });
    }
}
