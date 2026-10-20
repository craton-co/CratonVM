// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 23, lane L4: a record's generated equals /
// hashCode / toString (`java.lang.runtime.ObjectMethods`), which CratonVM
// answers natively (`invokedynamic.rs::execute_record_object_method`,
// `native-builtins/src/intrinsics/record.rs`). Checks the ORDER in which the
// components' own methods run (observable through side effects and through
// which exception escapes), float/double equality (`Float.compare`
// semantics: NaN equals NaN, 0.0 does not equal -0.0), null components, and
// the rendering of every primitive kind.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W23RecordObjectMethods
//
// HotSpot 25 (25.0.3) prints:
//   equals order: [b.equals, a.equals] -> true
//   equals stops: [b.equals] -> false
//   equals throws: IllegalStateException b
//   hashCode order: [a.hashCode, b.hashCode] -> 3105
//   toString order: [a.toString, b.toString] -> P[a=A, b=B]
//   nan equals: true
//   zero equals: false
//   nan hash equal: true
//   null components: N[s=null, o=null] 0 true
//   prims: Prims[z=true, b=-1, s=-2, c=x, i=-3, j=-4, f=1.5, d=-0.0]
//   prims hash: -1974433168
//   empty: E[] 0 true
//
// ObjectMethods builds equals as nested guardWithTest handles, the LAST
// component outermost, so HotSpot compares components last-to-first and
// stops at the first difference; hashCode and toString go first-to-last.

import java.util.ArrayList;
import java.util.List;

public class L4W23RecordObjectMethods {
    static final List<String> log = new ArrayList<>();

    static final class C {
        final String n;
        final boolean eq;
        final RuntimeException boom;
        C(String n, boolean eq, RuntimeException boom) { this.n = n; this.eq = eq; this.boom = boom; }
        @Override public boolean equals(Object o) {
            log.add(n + ".equals");
            if (boom != null) throw boom;
            return eq;
        }
        @Override public int hashCode() { log.add(n + ".hashCode"); return n.charAt(0); }
        @Override public String toString() { log.add(n + ".toString"); return n.toUpperCase(); }
    }

    record P(C a, C b) {}
    record F(float f, double d) {}
    record N(String s, Object o) {}
    record Prims(boolean z, byte b, short s, char c, int i, long j, float f, double d) {}
    record E() {}

    static String take(Object r) {
        String s = log + " -> " + r;
        log.clear();
        return s;
    }

    public static void main(String[] args) {
        C a1 = new C("a", true, null), b1 = new C("b", true, null);
        C a2 = new C("a", true, null), b2 = new C("b", true, null);
        boolean r = new P(a1, b1).equals(new P(a2, b2));
        System.out.println("equals order: " + take(r));

        C bNo = new C("b", false, null);
        r = new P(a1, bNo).equals(new P(a2, b2));
        System.out.println("equals stops: " + take(r));

        C aBoom = new C("a", true, new IllegalStateException("a"));
        C bBoom = new C("b", true, new IllegalStateException("b"));
        try {
            new P(aBoom, bBoom).equals(new P(a2, b2));
            System.out.println("equals throws: none");
        } catch (IllegalStateException e) {
            System.out.println("equals throws: IllegalStateException " + e.getMessage());
        }
        log.clear();

        int h = new P(a1, b1).hashCode();
        System.out.println("hashCode order: " + take(h));

        String s = new P(a1, b1).toString();
        System.out.println("toString order: " + take(s));

        System.out.println("nan equals: " + new F(Float.NaN, Double.NaN).equals(new F(Float.NaN, Double.NaN)));
        System.out.println("zero equals: " + new F(0.0f, 0.0).equals(new F(-0.0f, 0.0)));
        System.out.println("nan hash equal: "
                + (new F(Float.intBitsToFloat(0x7fc00001), 1).hashCode() == new F(Float.NaN, 1).hashCode()));
        N n = new N(null, null);
        System.out.println("null components: " + n + " " + n.hashCode() + " " + n.equals(new N(null, null)));
        Prims p = new Prims(true, (byte) -1, (short) -2, 'x', -3, -4L, 1.5f, -0.0);
        System.out.println("prims: " + p);
        System.out.println("prims hash: " + p.hashCode());
        E e = new E();
        System.out.println("empty: " + e + " " + e.hashCode() + " " + e.equals(new E()));
    }
}
