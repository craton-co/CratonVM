// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

/**
 * SBR round 5, lane fix3: HotSpot-diffed probe of IndirectVarHandle.withInvokeExactBehavior()
 * (MethodHandles.filterValue handles) as the FIRST exact VarHandle of the run, before any
 * other exact handle exists: a cold refused call, a call site JIT-warmed on the plain
 * handle and then given the exact one, matching exact calls, the withInvokeBehavior() round
 * trip, and the same after garbage collections. Run in both modes.
 *
 * Run: javac -d out Sbr5Fix3IndirectExact.java
 *      cratonvm --java-home <jdk25> [--compatible] -cp out Sbr5Fix3IndirectExact
 */
public class Sbr5Fix3IndirectExact {
    long l = 40;
    static long id(long x) { return x; }

    interface Body { Object run() throws Throwable; }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + ": " + out);
    }

    static Object readObj(VarHandle h, Sbr5Fix3IndirectExact o) { return h.get(o); }
    static long readLong(VarHandle h, Sbr5Fix3IndirectExact o) { return (long) h.get(o); }
    static void writeObj(VarHandle h, Sbr5Fix3IndirectExact o, Object v) { h.set(o, v); }
    static void writeLong(VarHandle h, Sbr5Fix3IndirectExact o, long v) { h.set(o, v); }

    public static void main(String[] args) throws Throwable {
        Class<?> me = Sbr5Fix3IndirectExact.class;
        MethodHandles.Lookup lk = MethodHandles.lookup();
        VarHandle vl = lk.findVarHandle(me, "l", long.class);
        MethodHandle id = lk.findStatic(me, "id", MethodType.methodType(long.class, long.class));
        VarHandle plain = MethodHandles.filterValue(vl, id, id);
        Sbr5Fix3IndirectExact o = new Sbr5Fix3IndirectExact();
        // Warm the plain handle through each shape first (JIT-compiles the callers).
        long sum = 0;
        for (int i = 0; i < 30000; i++) {
            sum += (Long) readObj(plain, o) + readLong(plain, o);
            writeObj(plain, o, (Object) 40L);
            writeLong(plain, o, 40L);
        }
        System.out.println("warm: " + sum);
        VarHandle exact = plain.withInvokeExactBehavior();
        System.out.println("exact: " + exact.hasInvokeExactBehavior() + " plain: " + plain.hasInvokeExactBehavior());
        row("exact-get-long", () -> readLong(exact, o));
        row("exact-get-object", () -> readObj(exact, o));
        row("exact-set-long", () -> { writeLong(exact, o, 7L); return o.l; });
        row("exact-set-object", () -> { writeObj(exact, o, (Object) 8L); return o.l; });
        row("exact-get-int-cast", () -> (int) exact.get(o));
        row("exact-get-void", () -> { exact.get(o); return "ok"; });
        row("plain-get-object", () -> readObj(plain, o));
        VarHandle back = exact.withInvokeBehavior();
        row("back-get-object", () -> readObj(back, o));
        row("back-exact", () -> back.hasInvokeExactBehavior());
        row("exact-twice", () -> exact.withInvokeExactBehavior().hasInvokeExactBehavior());
        for (int round = 0; round < 3; round++) {
            for (int i = 0; i < 20000; i++) {
                readLong(exact, o);
                writeLong(exact, o, 40L);
            }
            System.gc();
            final int r = round;
            row("exact-get-object-after-gc-" + r, () -> readObj(exact, o));
            row("exact-get-long-after-gc-" + r, () -> readLong(exact, o));
        }
        // A fresh exact handle built late, after a hot refused site.
        VarHandle late = MethodHandles.filterValue(vl, id, id).withInvokeExactBehavior();
        row("late-get-object", () -> readObj(late, o));
        row("late-get-long", () -> readLong(late, o));
    }
}
