// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L4 (review; page
// `i42-L4-reflective-switch-and-record-bootstraps-have-null-answering-bridges`):
// `SwitchBootstraps.typeSwitch` / `enumSwitch` and `ObjectMethods.bootstrap`
// CALLED as ordinary static methods (not through `invokedynamic`), as a
// framework that builds its own call sites does. HotSpot runs the JDK's
// bytecode: a working `ConstantCallSite`, a `MethodHandle` for a `MethodHandle`
// type descriptor. CratonVM registers a `Bridge` for all three
// (`native-builtins/src/phases_late/reflect_invoke.rs`,
// `register_p69_switch_bootstraps`) that answers `null`; whether it or the
// JDK bytecode answers depends on the door the call takes
// (`resolve_dispatch` step 3 lets bytecode win).
//
// Run: javac -d out L4W42ReflectiveSwitchBootstraps.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W42ReflectiveSwitchBootstraps
//
// Expected HotSpot 25 output (default and -Xint, measured locally):
//   typeSwitch-direct: 1 0 2 -1
//   typeSwitch-reflect: 1 0 2 -1
//   enumSwitch-direct: 1 0 2 -1
//   objectMethods-direct: R[a=1, b=x] 1
//   objectMethods-reflect: R[a=1, b=x]
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;
import java.lang.runtime.ObjectMethods;
import java.lang.runtime.SwitchBootstraps;

public class L4W42ReflectiveSwitchBootstraps {
    enum E {
        A,
        B
    }

    record R(int a, String b) {
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String typeSwitch(CallSite cs) throws Throwable {
        MethodHandle h = cs.getTarget();
        return (int) h.invokeExact((Object) "s", 0) + " " + (int) h.invokeExact((Object) 7, 0) + " "
                + (int) h.invokeExact((Object) 1.5, 0) + " " + (int) h.invokeExact((Object) null, 0);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType ts = MethodType.methodType(int.class, Object.class, int.class);
        row("typeSwitch-direct", () -> typeSwitch(
                SwitchBootstraps.typeSwitch(lookup, "typeSwitch", ts, Integer.class, String.class)));
        row("typeSwitch-reflect", () -> {
            Method m = SwitchBootstraps.class.getMethod("typeSwitch", MethodHandles.Lookup.class, String.class,
                    MethodType.class, Object[].class);
            return typeSwitch((CallSite) m.invoke(null, lookup, "typeSwitch", ts,
                    new Object[] {Integer.class, String.class}));
        });
        row("enumSwitch-direct", () -> {
            CallSite cs = SwitchBootstraps.enumSwitch(lookup, "enumSwitch",
                    MethodType.methodType(int.class, E.class, int.class), "B", "A");
            MethodHandle h = cs.getTarget();
            return (int) h.invokeExact(E.A, 0) + " " + (int) h.invokeExact(E.B, 0) + " "
                    + (int) h.invokeExact(E.B, 1) + " " + (int) h.invokeExact((E) null, 0);
        });
        MethodHandle ga = lookup.findGetter(R.class, "a", int.class);
        MethodHandle gb = lookup.findGetter(R.class, "b", String.class);
        row("objectMethods-direct", () -> {
            MethodHandle ts2 = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            MethodHandle hc = (MethodHandle) ObjectMethods.bootstrap(lookup, "hashCode", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            R r = new R(1, "x");
            return (String) ts2.invokeExact(r) + " " + ((int) hc.invokeExact(r) == r.hashCode() ? 1 : 0);
        });
        row("objectMethods-reflect", () -> {
            Method m = ObjectMethods.class.getMethod("bootstrap", MethodHandles.Lookup.class, String.class,
                    java.lang.invoke.TypeDescriptor.class, Class.class, String.class, MethodHandle[].class);
            MethodHandle ts2 = (MethodHandle) m.invoke(null, lookup, "toString", MethodHandle.class, R.class, "a;b",
                    new MethodHandle[] {ga, gb});
            return (String) ts2.invokeExact(new R(1, "x"));
        });
    }
}
