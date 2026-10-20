// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L4 (review): `CallSite.dynamicInvoker()`
// is an exact handle of the call site's type on HotSpot: `invokeExact` at
// another type is `WrongMethodTypeException: handle's method type ... but
// found ...`, and `invoke` converts. CratonVM's invoker carried its target's
// LEAF descriptor and was never judged, so a `(String)Object` or
// `(Object)String` call ran the target (`CRATONVM_MH_DYNAMIC_INVOKER_SITE_TYPE`).
// The first wrong row is repeated last.
//
// Run: javac -d out L4W59DynamicInvokerExact.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W59DynamicInvokerExact
//// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags and -Xint identical)
// exact-ok: x
// exact-string-arg: java.lang.invoke.WrongMethodTypeException: handle's method type (Object)Object but found (String)Object
// exact-string-ret: java.lang.invoke.WrongMethodTypeException: handle's method type (Object)Object but found (Object)String
// invoke-string: x
// invoke-int: 5
// int-exact: 8
// int-exact-long: java.lang.invoke.WrongMethodTypeException: handle's method type (int)int but found (int)long
// int-invoke-long: 8
// int-invoke-obj: 8
// setTarget-wrong: java.lang.invoke.WrongMethodTypeException: MethodHandle(Object)Object should be of type (int)int
// int-exact-again: 8
// exact-string-arg-again: java.lang.invoke.WrongMethodTypeException: handle's method type (Object)Object but found (String)Object
// HOTSPOT_EXPECTED_END
import java.lang.invoke.*;
public class L4W59DynamicInvokerExact {
  static Object id(Object o) { return o; }
  static int twice(int x) { return 2 * x; }
  interface B { Object run() throws Throwable; }
  static void row(String n, B b) { try { System.out.println(n + ": " + b.run()); } catch (Throwable t) { System.out.println(n + ": " + t); } }
  public static void main(String[] a) throws Throwable {
    var lk = MethodHandles.lookup();
    MethodType oo = MethodType.methodType(Object.class, Object.class);
    MutableCallSite cs = new MutableCallSite(oo); cs.setTarget(lk.findStatic(L4W59DynamicInvokerExact.class, "id", oo));
    MethodHandle di = cs.dynamicInvoker();
    row("exact-ok", () -> (Object) di.invokeExact((Object) "x"));
    row("exact-string-arg", () -> (Object) di.invokeExact("x"));
    row("exact-string-ret", () -> { String s = (String) di.invokeExact((Object) "x"); return s; });
    row("invoke-string", () -> (String) di.invoke("x"));
    row("invoke-int", () -> (int) di.invoke((Object) 5));
    MutableCallSite ci = new MutableCallSite(MethodType.methodType(int.class, int.class)); ci.setTarget(lk.findStatic(L4W59DynamicInvokerExact.class, "twice", MethodType.methodType(int.class, int.class)));
    MethodHandle dii = ci.dynamicInvoker();
    row("int-exact", () -> (int) dii.invokeExact(4));
    row("int-exact-long", () -> (long) dii.invokeExact(4));
    row("int-invoke-long", () -> (long) dii.invoke(4));
    row("int-invoke-obj", () -> (Object) dii.invoke((Object) 4));
    row("setTarget-wrong", () -> { ci.setTarget(lk.findStatic(L4W59DynamicInvokerExact.class, "id", oo)); return "set"; });
    row("int-exact-again", () -> (int) dii.invokeExact(4));
    row("exact-string-arg-again", () -> (Object) di.invokeExact("x"));
  }
}
