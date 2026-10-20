// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L4 -- a lambda proxy's method handles
// name their owner by class id (`CRATONVM_MH_INSTANCE_OWNER_ID`; proposal
// `i61-L4-proposal-method-handles-name-their-owner-by-class-id-20261025.md`,
// first stage) and so cast the receiver and crack as the proxy's member
// (`i61-L4-lambda-proxy-handles-check-no-receiver-and-do-not-crack-20261025.md`).
// The handle is minted over the class `""` (a proxy's mirror names no
// class): before the switch the door cast no receiver, `bindTo` bound any
// value, and `revealDirect` refused it as not direct. The rows the switch
// fixes come first and again at the end (cold-VM first-call gaps); the
// `named-*` rows are controls the switch must not move. The proxy's
// hidden-class name is masked as `P`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// bindTo-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// invoke-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// getter-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.String to P
// exact-wrong-site: java.lang.invoke.WrongMethodTypeException: handle's method type (P)int but found (Object)void
// exact-erased-site: java.lang.invoke.WrongMethodTypeException: handle's method type (P)int but found (Object)int
// revealDirect: invokeVirtual P.run:()void
// revealDirect-getter: getField P.arg$1:()int
// reflectAs: run
// invoke-right-receiver: ran
// bindTo-right-receiver: ran
// getter-right-receiver: 42
// named-invoke-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// named-invoke: 3
// named-bindTo-wrong-receiver: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
// warm: 4000
// bindTo-wrong-receiver-again: java.lang.ClassCastException: Cannot cast java.lang.String to P
// invoke-wrong-receiver-again: java.lang.ClassCastException: Cannot cast java.lang.String to P
// revealDirect-again: invokeVirtual P.run:()void
// exact-wrong-site-again: java.lang.invoke.WrongMethodTypeException: handle's method type (P)int but found (Object)void
// exact-erased-site-again: java.lang.invoke.WrongMethodTypeException: handle's method type (P)int but found (Object)int
// HOTSPOT_EXPECTED_END
//
// Switches: under `--jdk-only` the handles are the JDK's own and the
// `invoke` / `invokeExact` rows need `CRATONVM_MH_REAL_UNSPELLABLE_TYPE_ADAPTS`
// (default on); under `--compatible` every row but the `named-*` controls
// needs `CRATONVM_MH_INSTANCE_OWNER_ID` (on from wave 62's last commit).
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W62LambdaProxyOwnerCast`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.util.function.IntSupplier;

public class L4W62LambdaProxyOwnerCast {
    interface Row {
        Object run() throws Throwable;
    }

    static String[] masked = {"\0", "\0"};

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        for (String m : masked) {
            out = out.replace(m, "P");
        }
        System.out.println(name + ": " + out);
    }

    static IntSupplier capturing(int n) {
        return () -> n;
    }

    public static void main(String[] args) throws Throwable {
        Runnable r = () -> {};
        Class<?> proxy = r.getClass();
        IntSupplier s = capturing(42);
        Class<?> sp = s.getClass();
        masked = new String[] {proxy.getName(), sp.getName()};
        MethodHandles.Lookup l = MethodHandles.privateLookupIn(proxy, MethodHandles.lookup());
        MethodHandles.Lookup ls = MethodHandles.privateLookupIn(sp, MethodHandles.lookup());
        MethodHandle run = l.findVirtual(proxy, "run", MethodType.methodType(void.class));
        MethodHandle get = ls.findGetter(sp, "arg$1", int.class);
        row("bindTo-wrong-receiver", () -> run.bindTo("x"));
        row("invoke-wrong-receiver", () -> {
            run.invoke((Object) "x");
            return "ran";
        });
        row("getter-wrong-receiver", () -> (int) get.invoke((Object) "x"));
        row("exact-wrong-site", () -> {
            get.invokeExact((Object) s);
            return "ran";
        });
        row("exact-erased-site", () -> (int) get.invokeExact((Object) s));
        row("revealDirect", () -> l.revealDirect(run));
        row("revealDirect-getter", () -> ls.revealDirect(get));
        row("reflectAs", () -> MethodHandles.reflectAs(Method.class, run).getName());
        row("invoke-right-receiver", () -> {
            run.invoke((Object) r);
            return "ran";
        });
        row("bindTo-right-receiver", () -> {
            run.bindTo(r).invoke();
            return "ran";
        });
        row("getter-right-receiver", () -> (int) get.invoke((Object) s));
        MethodHandle length = MethodHandles.lookup().findVirtual(String.class, "length", MethodType.methodType(int.class));
        row("named-invoke-wrong-receiver", () -> (int) length.invoke((Object) Integer.valueOf(3)));
        row("named-invoke", () -> (int) length.invoke((Object) "abc"));
        row("named-bindTo-wrong-receiver", () -> length.bindTo(Integer.valueOf(3)));
        long n = 0;
        for (int i = 0; i < 2000; i++) {
            run.invoke((Object) r);
            n += (int) get.invoke((Object) s) == 42 ? 1 : 0;
            n += (int) length.invoke((Object) "a");
        }
        System.out.println("warm: " + n);
        row("bindTo-wrong-receiver-again", () -> run.bindTo("y"));
        row("invoke-wrong-receiver-again", () -> {
            run.invoke((Object) "y");
            return "ran";
        });
        row("revealDirect-again", () -> l.revealDirect(run));
        row("exact-wrong-site-again", () -> {
            get.invokeExact((Object) s);
            return "ran";
        });
        row("exact-erased-site-again", () -> (int) get.invokeExact((Object) s));
    }
}
