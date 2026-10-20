// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L4: the measurement gate of
// `CRATONVM_MH_INVOKE_RETURN_UNBOX` (a reference return unboxed at a primitive
// `invoke` site as `asType` does). Groovy's `IndyInterface` once died of a
// strict rule on this path, so the switch is measured on the shapes it uses
// before it is turned on:
//
// * part 1 (JDK only): a Groovy-style `MutableCallSite` whose target is a
//   `()Object`-returning selector `asType`d to the site's PRIMITIVE type,
//   behind a `guardWithTest`, called through `dynamicInvoker().invokeExact`
//   and through `invoke` at primitive sites, in loops (counted in `long`);
// * part 2 (only when a Groovy jar is on the class path, reflectively): a
//   GroovyShell script with int-typed methods, closures, a `sum()` over
//   boxed values and mixed-width `+=` (`def m = 5L; m += 1`, whose FIRST call
//   runs Groovy's `explicitCastArguments(filterArguments(...))` chain),
//   whose call sites go through IndyInterface. CratonVM (wave 57) failed it
//   with `WrongMethodTypeException` (`guardWithTest`'s type) and answered
//   `m` = 5, `d` = 1.5 (one stamp over an adapter; wave 58,
//   `CRATONVM_MH_GUARD_TYPE_IS_TARGETS`, `CRATONVM_MH_ADAPTER_STAMP_CONVERTS`).
//   Prints `gr.present = MISSING` without the jar.
//
// Run: javac -d out L4W58GroovyStyleInvoke.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out[;groovy-5.x.jar] L4W58GroovyStyleInvoke
//
// Expected HotSpot 25 output: see the block at the end of this file (with
// groovy-5.0.6.jar on the class path; without it the last line reads
// `gr.present = MISSING`).
import java.lang.invoke.*;

public class L4W58GroovyStyleInvoke {
    static Object select(Object receiver) { return ((Integer) receiver) * 2; }
    static Object selectLong(Object receiver) { return ((Integer) receiver).longValue() * 3; }
    static boolean isInt(Object receiver) { return receiver instanceof Integer; }
    static Object fallback(Object receiver) { return -1; }

    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodType objObj = MethodType.methodType(Object.class, Object.class);
        MethodHandle select = lk.findStatic(L4W58GroovyStyleInvoke.class, "select", objObj);
        MethodHandle selectLong = lk.findStatic(L4W58GroovyStyleInvoke.class, "selectLong", objObj);
        MethodHandle test = lk.findStatic(L4W58GroovyStyleInvoke.class, "isInt", MethodType.methodType(boolean.class, Object.class));
        MethodHandle fallback = lk.findStatic(L4W58GroovyStyleInvoke.class, "fallback", objObj);
        MethodType siteType = MethodType.methodType(int.class, Object.class);
        MutableCallSite cs = new MutableCallSite(siteType);
        cs.setTarget(MethodHandles.guardWithTest(test, select, fallback).asType(siteType));
        MethodHandle invoker = cs.dynamicInvoker();
        long sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += (int) invoker.invokeExact((Object) Integer.valueOf(i));
        }
        System.out.println("callsite-exact: " + sum);
        MethodHandle gwt = MethodHandles.guardWithTest(test, select, fallback);
        sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += (int) gwt.invoke((Object) Integer.valueOf(i));
        }
        System.out.println("gwt-invoke-int: " + sum);
        sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += (long) selectLong.invoke((Object) Integer.valueOf(i));
        }
        System.out.println("invoke-long: " + sum);
        sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += (int) gwt.invoke((Object) "s");
        }
        System.out.println("gwt-fallback-int: " + sum);
        MethodHandle widened = select.asType(siteType);
        sum = 0;
        for (int i = 0; i < 20000; i++) {
            sum += (long) widened.invoke((Object) Integer.valueOf(i));
        }
        System.out.println("astype-invoke-long: " + sum);
        groovy();
    }

    static void groovy() {
        Class<?> shell;
        try {
            shell = Class.forName("groovy.lang.GroovyShell");
        } catch (Throwable t) {
            System.out.println("gr.present = MISSING");
            return;
        }
        String script = String.join("\n",
            "int twice(int x) { x * 2 }",
            "long acc = 0",
            "for (int i = 0; i < 20000; i++) { acc += twice(i) }",
            "def c = { int v -> v + 1 }",
            "int s = 0",
            "for (int i = 0; i < 20000; i++) { s += c(i) }",
            "int total = [a: 1, b: 2, c: 3].values().sum()",
            "Integer maybe = null",
            "def m = 5L",
            "m += 1",
            "double d = 1.5",
            "d += 1",
            "int folded = (1..100).inject(0) { int x, int y -> x + y }",
            "\"${acc} ${s} ${total} ${maybe} ${folded} ${m} ${d}\"");
        try {
            Object sh = shell.getConstructor().newInstance();
            Object out = shell.getMethod("evaluate", String.class).invoke(sh, script);
            System.out.println("gr.script = " + out);
        } catch (Throwable t) {
            Throwable root = t;
            while (root.getCause() != null && root.getCause() != root) root = root.getCause();
            System.out.println("gr.script = throws " + root.getClass().getName());
        }
    }
}


// Expected HotSpot 25 output (Temurin 25.0.3, default and -Xint identical,
// groovy-5.0.6.jar on the class path):
//   callsite-exact: 399980000
//   gwt-invoke-int: 399980000
//   invoke-long: 599970000
//   gwt-fallback-int: -20000
//   astype-invoke-long: 399980000
//   gr.script = 399980000 200010000 6 null 5050 6 2.5
