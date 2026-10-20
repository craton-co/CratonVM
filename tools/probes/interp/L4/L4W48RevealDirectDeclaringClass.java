// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L4 (bug hunt): `Lookup.revealDirect`
// reports the member's DECLARING class, as HotSpot's resolved `MemberName`
// has it. CratonVM reported the finder's `refc` (`Sub` for a method, static
// or field `Base` declares; `I` and `invokeInterface` for an interface's
// `Object` method). The JDK's own `LambdaMetafactory` reads it too (its
// `Incorrect number of parameters for instance method invokeVirtual
// <declaring class>...` text).
//
// Positive control: row `virtual` (the base printed `L4W48RevealDirectDeclaringClass$Sub.m`).
//
// Run: javac -d out L4W48RevealDirectDeclaringClass.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible | --jdk-only] -cp out L4W48RevealDirectDeclaringClass
//
// Expected HotSpot 25 output (default and -Xint, identical):
//   virtual: invokeVirtual L4W48RevealDirectDeclaringClass$Base.m:()String
//   static: invokeStatic L4W48RevealDirectDeclaringClass$Base.s:()String
//   getter: getField L4W48RevealDirectDeclaringClass$Base.f:()int
//   sgetter: getStatic L4W48RevealDirectDeclaringClass$Base.sf:()int
//   default: invokeVirtual L4W48RevealDirectDeclaringClass$Impl.d:()String
//   object: invokeVirtual java.lang.Object.hashCode:()int
//   iface-object: invokeVirtual java.lang.Object.hashCode:()int
import java.lang.invoke.*;

public class L4W48RevealDirectDeclaringClass {
    public static class Base {
        public String m() { return "b"; }
        public static String s() { return "s"; }
        public int f;
        public static int sf;
    }
    public static class Sub extends Base { }
    interface I { default String d() { return "d"; } }
    public static class Impl implements I { }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType s = MethodType.methodType(String.class);
        System.out.println("virtual: " + l.revealDirect(l.findVirtual(Sub.class, "m", s)));
        System.out.println("static: " + l.revealDirect(l.findStatic(Sub.class, "s", s)));
        System.out.println("getter: " + l.revealDirect(l.findGetter(Sub.class, "f", int.class)));
        System.out.println("sgetter: " + l.revealDirect(l.findStaticGetter(Sub.class, "sf", int.class)));
        System.out.println("default: " + l.revealDirect(l.findVirtual(Impl.class, "d", s)));
        System.out.println("object: " + l.revealDirect(l.findVirtual(Sub.class, "hashCode", MethodType.methodType(int.class))));
        System.out.println("iface-object: " + l.revealDirect(l.findVirtual(I.class, "hashCode", MethodType.methodType(int.class))));
    }
}
