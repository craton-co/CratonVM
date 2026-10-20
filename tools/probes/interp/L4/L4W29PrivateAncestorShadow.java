// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 29, lane L4: a private method of a grand-
// superclass with the same name and descriptor as a method an intermediate
// abstract class re-declares abstract (javac output, no hand assembly).
// `this.m()` in `C` is `invokevirtual C.m`; JVMS §5.4.3.3 resolves it to the
// FIRST declaration on the superclass chain, B's abstract public `m`, and
// §5.4.6 selects D.m. CratonVM decided "is this a private call?" with
// `find_method_recursive`, whose first phase steps past an abstract
// declaration to a concrete ancestor: it found A's private `m` and pinned the
// site to it (`invoke.rs::resolved_private_invokevirtual_target`, and the JIT
// doors through `classloading::invokevirtual_private_declaring_class`), so
// rows v1.t / v1.u printed A.m on every round. Now both take the resolved
// method (`resolve::selection::resolve_declaring` / the same chain walk).
// v2 is the control where no declaration is in between: the resolved method
// IS A2's private `m` (the classes are nestmates, so it is accessible), and
// HotSpot runs it.
//
// Each row runs three cold rounds, then 20000 warm calls through the caches
// and the JIT; `warm` lists every distinct answer seen.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W29PrivateAncestorShadow
// (the same lines in every mode)
//
// HotSpot 25 (25.0.3, default and -Xint) prints exactly:
//   v1.t#1: D.m
//   v1.u#1: D.m
//   v1.viaA#1: A.m
//   v2.t#1: A2.m
//   v1.t#2: D.m
//   v1.u#2: D.m
//   v1.viaA#2: A.m
//   v2.t#2: A2.m
//   v1.t#3: D.m
//   v1.u#3: D.m
//   v1.viaA#3: A.m
//   v2.t#3: A2.m
//   warm: [D.m, A2.m]

public class L4W29PrivateAncestorShadow {
    static class A { private String m() { return "A.m"; } String viaA() { return m(); } }
    static abstract class B extends A { public abstract String m(); }
    static abstract class C extends B { String t() { return m(); } String u(C c) { return c.m(); } }
    static class D extends C { public String m() { return "D.m"; } }

    interface I { String m(); }
    static class A2 { private String m() { return "A2.m"; } }
    static abstract class B2 extends A2 implements I { }
    static abstract class C2 extends B2 { String t() { return m(); } }
    static class D2 extends C2 { public String m() { return "D2.m"; } }

    static String once(java.util.function.Supplier<String> s) {
        try { return s.get(); } catch (Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    }
    public static void main(String[] args) {
        D d = new D();
        D2 d2 = new D2();
        for (int r = 1; r <= 3; r++) {
            System.out.println("v1.t#" + r + ": " + once(d::t));
            System.out.println("v1.u#" + r + ": " + once(() -> d.u(d)));
            System.out.println("v1.viaA#" + r + ": " + once(d::viaA));
            System.out.println("v2.t#" + r + ": " + once(d2::t));
        }
        java.util.Set<String> w = new java.util.LinkedHashSet<>();
        for (int i = 0; i < 20000; i++) { w.add(once(d::t)); w.add(once(() -> d.u(d))); w.add(once(d2::t)); }
        System.out.println("warm: " + w);
    }
}
