// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L4: a `MutableCallSite` / `VolatileCallSite`
// built from a `MethodType` has, until its first `setTarget`, the target
// `CallSite.makeUninitializedCallSite` makes, which throws
// `IllegalStateException: uninitialized call site` when invoked (JDK 25's
// `CallSite.uninitializedCallSite`). CratonVM's placeholder is an
// `MH_KIND_CONSTANT` shim that answered `null` (or zero), so a program that
// invoked a call site before setting it went on with a made-up value.
//
// Positive control: row `mutable-dyn` (the base printed `null`).
//
// Run: javac -d out L4W47UninitializedCallSite.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W47UninitializedCallSite
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   mutable-dyn: java.lang.IllegalStateException: uninitialized call site
//   mutable-target: java.lang.IllegalStateException: uninitialized call site
//   volatile-int: java.lang.IllegalStateException: uninitialized call site
//   mutable-args: java.lang.IllegalStateException: uninitialized call site
//   after-set: 42
//   type: ()int (String,int)void
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.VolatileCallSite;

public class L4W47UninitializedCallSite {
    interface Body {
        Object run() throws Throwable;
    }

    static void row(String name, Body body) {
        String out;
        try {
            out = String.valueOf(body.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        row("mutable-dyn", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(String.class));
            return (String) cs.dynamicInvoker().invokeExact();
        });
        row("mutable-target", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(Object.class));
            return cs.getTarget().invoke();
        });
        row("volatile-int", () -> {
            VolatileCallSite cs = new VolatileCallSite(MethodType.methodType(int.class));
            return (int) cs.dynamicInvoker().invokeExact();
        });
        row("mutable-args", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(void.class, String.class, int.class));
            cs.dynamicInvoker().invokeExact("x", 1);
            return "returned";
        });
        row("after-set", () -> {
            MutableCallSite cs = new MutableCallSite(MethodType.methodType(int.class));
            MethodHandle invoker = cs.dynamicInvoker();
            cs.setTarget(MethodHandles.constant(int.class, 42));
            return (int) invoker.invokeExact();
        });
        row("type", () -> new MutableCallSite(MethodType.methodType(int.class)).type() + " "
                + new MutableCallSite(MethodType.methodType(void.class, String.class, int.class)).getTarget().type());
    }
}
