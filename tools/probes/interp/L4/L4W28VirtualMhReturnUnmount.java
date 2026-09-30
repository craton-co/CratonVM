// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L4 (stage 1 of
// docs/known-issues/interpreter/i26-L4-proposal-let-a-virtual-thread-unmount-under-the-method-handle-natives-20260928.md):
// a virtual thread that sleeps inside a method-handle leaf whose PRIMITIVE
// result the door boxes or widens (or hands back as the same primitive, or a
// `void` to a `void` site) now unmounts beneath the door, as on HotSpot
// (method handles have no native frame there), and the remounted leaf frame
// makes the door's conversion. Every row must print HotSpot's value; a lost
// conversion shows as a raw `int` in an `Object` slot (a garbage or crashed
// row) or a wrong-typed value.
//
// Positive control (the mechanism MUST engage): run with
// `CRATONVM_DBG_MH_DISPATCH=1` and grep stderr for `[MH_TAIL_ADAPTER]`: rows
// 2, 3, 5, 6, 7, 9 and 10 each record one (`caller_return=Ljava/lang/Object;`,
// row 3 `caller_return=J`; row 7 under `--jdk-only` only), so seven lines
// (six under `--compatible`); rows 1 and 4 unmount without one (same type).
// Row 8 (a `void` leaf behind `invokeWithArguments`' `Object`) still pins --
// no record -- because the interpreter's `return` arm consults no conversion.
// Before wave 28 no row recorded one: rows 1-7, 9 and 10 pinned their
// carrier, with the same printed values.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W28VirtualMhReturnUnmount
// (row 7 goes through `Method.invoke`, a method handle only under the default
// `--jdk-only`; under `--compatible` it is the reflection native and pins.)
//
// HotSpot 25 (25.0.3, default and -Xint) prints:
//   1 invokeExact int site int: 7
//   2 invokeExact asType Object: 7 java.lang.Integer
//   3 invokeExact asType long: 7 java.lang.Long
//   4 invokeExact void site void: done
//   5 invoke Object site: 7 java.lang.Integer
//   6 invokeWithArguments int: 7 java.lang.Integer
//   7 Method.invoke int: 7 java.lang.Integer
//   8 invokeWithArguments void: null
//   9 insertArguments invoke Object: 12 java.lang.Integer
//   10 exactInvoker invokeExact Object: 7 java.lang.Integer
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.concurrent.atomic.AtomicReference;

public class L4W28VirtualMhReturnUnmount {
    interface Body {
        Object run() throws Throwable;
    }

    public static int count() throws InterruptedException {
        Thread.sleep(10);
        return 7;
    }

    public static int plus(int a, int b) throws InterruptedException {
        Thread.sleep(10);
        return a + b;
    }

    public static void work() throws InterruptedException {
        Thread.sleep(10);
    }

    static String describe(Object r) {
        return r == null ? "null" : r + " " + r.getClass().getName();
    }

    static void row(String label, Body body) throws InterruptedException {
        AtomicReference<String> out = new AtomicReference<>("(no answer)");
        Thread t = Thread.ofVirtual().start(() -> {
            try {
                out.set(String.valueOf(body.run()));
            } catch (Throwable e) {
                out.set(e.toString());
            }
        });
        t.join(20_000);
        System.out.println(label + ": " + (t.isAlive() ? "(still running)" : out.get()));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W28VirtualMhReturnUnmount.class;
        MethodHandle countH = l.findStatic(me, "count", MethodType.methodType(int.class));
        MethodHandle countObj = countH.asType(MethodType.methodType(Object.class));
        MethodHandle countLong = countH.asType(MethodType.methodType(long.class));
        MethodHandle workH = l.findStatic(me, "work", MethodType.methodType(void.class));
        MethodHandle plusH = l.findStatic(me, "plus", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle plus5 = MethodHandles.insertArguments(plusH, 0, 5);
        MethodHandle exObj = MethodHandles.exactInvoker(MethodType.methodType(Object.class));
        Method countM = me.getMethod("count");

        row("1 invokeExact int site int", () -> (int) countH.invokeExact());
        row("2 invokeExact asType Object", () -> describe((Object) countObj.invokeExact()));
        row("3 invokeExact asType long", () -> describe((long) countLong.invokeExact()));
        row("4 invokeExact void site void", () -> {
            workH.invokeExact();
            return "done";
        });
        row("5 invoke Object site", () -> describe((Object) countH.invoke()));
        row("6 invokeWithArguments int", () -> describe(countH.invokeWithArguments()));
        row("7 Method.invoke int", () -> describe(countM.invoke(null)));
        row("8 invokeWithArguments void", () -> String.valueOf(workH.invokeWithArguments()));
        row("9 insertArguments invoke Object", () -> describe((Object) plus5.invoke(7)));
        row("10 exactInvoker invokeExact Object", () -> describe((Object) exObj.invokeExact(countObj)));
    }
}
