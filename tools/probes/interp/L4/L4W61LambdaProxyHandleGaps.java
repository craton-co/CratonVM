// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 61, lane L4 -- the gaps left open by the
// lambda-proxy member handles (`L4W61LambdaProxyMemberHandles`,
// `L4W61LambdaProxyFieldHandles`), filed as
// `docs/internal/fixed-bugs/interpreter-i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack-FIXED-20261112.md`.
// A proxy's handle is minted over the class `""` (a proxy's mirror names no
// class), so the door casts no receiver to the proxy, `revealDirect` /
// `reflectAs` cannot crack it, and a `VarHandle` over a capture has no
// coordinate. Row `public-lookup` (a refusal naming the proxy's class, which
// printed `?`) is fixed in wave 61 and kept as a regression row. The proxy's
// hidden-class name is masked as `P`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint)
// revealDirect: invokeVirtual P.run:()void
// reflectAs: run
// bindTo-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// invoke-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// public-lookup: java.lang.IllegalAccessException: symbolic reference class is not accessible: class P, from public Lookup
// findVarHandle: [class P] 42
// findVarHandle-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// revealDirect-again: invokeVirtual P.run:()void
// findVarHandle-read-only: false true
// findVarHandle-second-capture: 42 7
// findVarHandle-wrong-type: java.lang.NoSuchFieldException: no such field: P.arg$1/long/getField
// HOTSPOT_EXPECTED_END
//
// Wave 79 (lane L4): `--compatible` mints the capture's `VarHandle` over the
// proxy (`findVarHandle`, `findVarHandle-wrong-receiver`); `--jdk-only`'s
// `findVarHandle` row (a read-write handle reading `null`) is still open.
//
// Wave 80 (lane L4): `--jdk-only` -- `MethodHandleNatives.resolve` resolves a
// proxy's capture by name and type with its `private final` flags (and
// `TRUSTED_FINAL`), `objectFieldOffset` answers the capture's slot, and the
// VarHandle door adopts the JDK's `FieldInstanceReadOnly` over the proxy. Rows
// `findVarHandle-read-only` (SET unsupported), `findVarHandle-second-capture`
// (the second capture's slot, then the first's) and `findVarHandle-wrong-type`
// (resolution by type) are new; the three rows from `findVarHandle` on
// should now be SAME in both modes.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W61LambdaProxyHandleGaps`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Method;
import java.util.function.IntSupplier;

public class L4W61LambdaProxyHandleGaps {
    interface Row {
        Object run() throws Throwable;
    }

    static String proxyName = "\0";

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out.replace(proxyName, "P"));
    }

    static IntSupplier capturing(int n) {
        return () -> n;
    }

    // Two int captures: `arg$1` is `a` (javac captures in order of first use).
    static IntSupplier capturing2(int a, int n) {
        return () -> a * 1000 + n;
    }

    public static void main(String[] args) throws Throwable {
        Runnable r = () -> {};
        Class<?> proxy = r.getClass();
        proxyName = proxy.getName();
        MethodHandles.Lookup l = MethodHandles.privateLookupIn(proxy, MethodHandles.lookup());
        MethodHandle run = l.findVirtual(proxy, "run", MethodType.methodType(void.class));
        row("revealDirect", () -> l.revealDirect(run));
        row("reflectAs", () -> MethodHandles.reflectAs(Method.class, run).getName());
        row("bindTo-wrong-receiver", () -> run.bindTo("x"));
        row("invoke-wrong-receiver", () -> {
            run.invoke((Object) "x");
            return "ran";
        });
        row("public-lookup", () -> MethodHandles.publicLookup().findVirtual(proxy, "run", MethodType.methodType(void.class)));
        IntSupplier s = capturing(42);
        Class<?> sp = s.getClass();
        MethodHandles.Lookup ls = MethodHandles.privateLookupIn(sp, MethodHandles.lookup());
        row("findVarHandle", () -> {
            VarHandle vh = ls.findVarHandle(sp, "arg$1", int.class);
            return vh.coordinateTypes().toString().replace(sp.getName(), "P") + " " + vh.get(s);
        });
        row("findVarHandle-wrong-receiver", () -> {
            VarHandle vh = ls.findVarHandle(sp, "arg$1", int.class);
            try {
                return vh.get((Object) "x");
            } catch (Throwable t) {
                // The capturing proxy is not `proxyName`'s: mask its name too.
                return String.valueOf(t).replace(sp.getName(), "P");
            }
        });
        row("revealDirect-again", () -> l.revealDirect(run));
        row("findVarHandle-read-only", () -> {
            VarHandle vh = ls.findVarHandle(sp, "arg$1", int.class);
            return vh.isAccessModeSupported(VarHandle.AccessMode.SET) + " "
                    + vh.isAccessModeSupported(VarHandle.AccessMode.GET);
        });
        IntSupplier s2 = capturing2(7, 42);
        Class<?> sp2 = s2.getClass();
        MethodHandles.Lookup ls2 = MethodHandles.privateLookupIn(sp2, MethodHandles.lookup());
        row("findVarHandle-second-capture", () -> {
            VarHandle second = ls2.findVarHandle(sp2, "arg$2", int.class);
            VarHandle first = ls2.findVarHandle(sp2, "arg$1", int.class);
            return second.get(s2) + " " + first.get(s2);
        });
        row("findVarHandle-wrong-type", () -> {
            try {
                return ls.findVarHandle(sp, "arg$1", long.class);
            } catch (ReflectiveOperationException e) {
                return e.toString().replace(sp.getName(), "P");
            }
        });
    }
}
