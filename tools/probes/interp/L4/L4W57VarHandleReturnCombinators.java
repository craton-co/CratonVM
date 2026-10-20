// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L4: `asType`'s return direction (see
// `L4W57VarHandleReturnConversions`) for the handles no side-table row
// describes: an `IndirectVarHandle` (`MethodHandles.filterValue`), judged by
// its own `value`, and an FFM layout handle (`ValueLayout.varHandle()`, a
// real `SegmentVarHandle` under `--jdk-only`, a synthetic layout under
// `--compatible`), judged by its form's get type. A refused `getAndSet`
// writes nothing. Before the fix: `0` (switch off); with the first cut of the
// switch, `ClassCastException` for the `--jdk-only` FFM rows and the indirect
// rows (the run-time unboxing reached a box the native had not judged), and
// the indirect `getAndSet` stored. Page
// `docs/internal/fixed-bugs/interpreter-L4-a-varhandle-read-into-a-narrower-primitive-site-answers-zero-FIXED-20261021.md`.
//
// Run: javac -d out L4W57VarHandleReturnCombinators.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W57VarHandleReturnCombinators
//
// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical):
//   indirect-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnCombinators)long to (VarHandle,L4W57VarHandleReturnCombinators)int
//   indirect-long-as-long: 40
//   indirect-int-as-long: 4
//   indirect-int-as-string: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnCombinators)int to (VarHandle,L4W57VarHandleReturnCombinators)String
//   indirect-getandset-long-as-int: java.lang.invoke.WrongMethodTypeException 40
//   ffm-long-as-int: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,MemorySegment,long)long to (VarHandle,MemorySegment,long)int
//   ffm-long-as-long: 40
//   ffm-int-as-long: 40
//   ffm-int-as-short: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,MemorySegment,long)int to (VarHandle,MemorySegment,long)short
//   indirect-long-as-int-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,L4W57VarHandleReturnCombinators)long to (VarHandle,L4W57VarHandleReturnCombinators)int
//   ffm-long-as-int-again: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(VarHandle,MemorySegment,long)long to (VarHandle,MemorySegment,long)int
import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W57VarHandleReturnCombinators {
    long l = 40;
    int i = 4;

    static long id(long x) {
        return x;
    }

    static int idi(int x) {
        return x;
    }

    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        Class<?> me = L4W57VarHandleReturnCombinators.class;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle vl = lk.findVarHandle(me, "l", long.class);
        VarHandle vi = lk.findVarHandle(me, "i", int.class);
        MethodHandle id = lk.findStatic(me, "id", MethodType.methodType(long.class, long.class));
        MethodHandle idi = lk.findStatic(me, "idi", MethodType.methodType(int.class, int.class));
        VarHandle fl = MethodHandles.filterValue(vl, id, id);
        VarHandle fi = MethodHandles.filterValue(vi, idi, idi);
        L4W57VarHandleReturnCombinators o = new L4W57VarHandleReturnCombinators();
        row("indirect-long-as-int", () -> (int) fl.get(o));
        row("indirect-long-as-long", () -> (long) fl.get(o));
        row("indirect-int-as-long", () -> (long) fi.get(o));
        row("indirect-int-as-string", () -> (String) fi.get(o));
        row("indirect-getandset-long-as-int", () -> {
            String r;
            try {
                r = String.valueOf((int) fl.getAndSet(o, 3L));
            } catch (Throwable t) {
                r = t.getClass().getName();
            }
            return r + " " + o.l;
        });
        try (Arena arena = Arena.ofConfined()) {
            MemorySegment seg = arena.allocate(16);
            VarHandle lv = ValueLayout.JAVA_LONG.varHandle();
            VarHandle iv = ValueLayout.JAVA_INT.varHandle();
            lv.set(seg, 0L, 40L);
            row("ffm-long-as-int", () -> (int) lv.get(seg, 0L));
            row("ffm-long-as-long", () -> (long) lv.get(seg, 0L));
            row("ffm-int-as-long", () -> (long) iv.get(seg, 0L));
            row("ffm-int-as-short", () -> (short) iv.get(seg, 0L));
            row("indirect-long-as-int-again", () -> (int) fl.get(o));
            row("ffm-long-as-int-again", () -> (int) lv.get(seg, 0L));
        }
    }
}
