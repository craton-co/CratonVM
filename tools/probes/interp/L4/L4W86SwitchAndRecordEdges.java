// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 86, lane L4: javac 25 pattern switches
// (`SwitchBootstraps.typeSwitch` / `enumSwitch` sites, restart index after a
// failed guard, `case null`, record deconstruction whose accessor throws ->
// `MatchException` with that cause, nested record patterns, a sealed
// hierarchy with qualified enum constants) and record `ObjectMethods`
// (`equals` / `hashCode` / `toString`): float and double components compared
// as `Float.compare == 0` (NaN equal to NaN, 0.0 not equal to -0.0), arrays by
// reference, `toString` with `Float.toString` / `Double.toString` / `char` /
// `byte` rendering and an empty record. An array's identity hash in
// `toString` is masked as `@H`.
//
// Run: javac -d . L4W86SwitchAndRecordEdges.java
//      cratonvm --jdk-only [--nojit] -cp . L4W86SwitchAndRecordEdges
//      cratonvm --compatible [--nojit] -cp . L4W86SwitchAndRecordEdges
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// guards: null|empty|short:ab|long:abcdef|int-neg|int:7|other:1.5
// enum-qualified: A1|B2|other-B:B3|other-A:A2
// nested: point-origin|point:1,2|line:0,0->3,4|line-other|default
// match-exception: java.lang.MatchException cause=java.lang.IllegalStateException: boom
// match-exception-null-arm: null-arm
// enum-switch: one|two|three-guarded|three|null
// rec-float: nan=true zero=false same=true hash-nan=true
// rec-double: nan=true zero=false
// rec-array: same-array=true equal-content=false hash-same=true
// rec-toString: F[f=NaN, d=-0.0, c=x, b=-1, z=true, l=9007199254740993, s=null]
// rec-array-toString: A[a=[I@H, s=x]
// rec-empty: E[] hash=0 equals=true
// rec-nested: O[i=I[v=1.0E10], name=n] equals=true
// HOTSPOT_EXPECTED_END

public class L4W86SwitchAndRecordEdges {
    sealed interface Shape permits P, L {
    }

    record P(int x, int y) implements Shape {
    }

    record L(P a, P b) implements Shape {
    }

    enum A { A1, A2 }

    enum B { B1, B2, B3 }

    sealed interface AB permits AW, BW {
    }

    record AW(A a) implements AB {
    }

    record BW(B b) implements AB {
    }

    enum N { ONE, TWO, THREE }

    record Bad(int v) {
        @Override
        public int v() {
            throw new IllegalStateException("boom");
        }
    }

    record F(float f, double d, char c, byte b, boolean z, long l, String s) {
    }

    record A2(int[] a, String s) {
    }

    record E() {
    }

    record I(double v) {
    }

    record O(I i, String name) {
    }

    record FF(float v) {
    }

    record DD(double v) {
    }

    static String guards(Object o) {
        return switch (o) {
            case null -> "null";
            case String s when s.isEmpty() -> "empty";
            case String s when s.length() < 3 -> "short:" + s;
            case String s -> "long:" + s;
            case Integer i when i < 0 -> "int-neg";
            case Integer i -> "int:" + i;
            default -> "other:" + o;
        };
    }

    static String enumQualified(Object o) {
        return switch (o) {
            case A a when a == A.A1 -> "A1";
            case B b when b == B.B2 -> "B2";
            case B b -> "other-B:" + b;
            case A a -> "other-A:" + a;
            default -> "?";
        };
    }

    static String nested(Object o) {
        return switch (o) {
            case P(int x, int y) when x == 0 && y == 0 -> "point-origin";
            case P(int x, int y) -> "point:" + x + "," + y;
            case L(P(var x1, var y1), P(var x2, var y2)) when x1 == 0 -> "line:" + x1 + "," + y1 + "->" + x2 + "," + y2;
            case L l -> "line-other";
            default -> "default";
        };
    }

    static String bad(Object o) {
        return switch (o) {
            case null -> "null-arm";
            case Bad(int v) -> "v=" + v;
            default -> "default";
        };
    }

    static String enumSwitch(N n, boolean guard) {
        return switch (n) {
            case null -> "null";
            case ONE -> "one";
            case TWO -> "two";
            case N x when guard && x == N.THREE -> "three-guarded";
            case THREE -> "three";
        };
    }

    public static void main(String[] args) {
        Object[] gs = {null, "", "ab", "abcdef", -3, 7, 1.5};
        StringBuilder sb = new StringBuilder();
        for (Object o : gs) {
            sb.append(sb.length() > 0 ? "|" : "").append(guards(o));
        }
        System.out.println("guards: " + sb);

        sb.setLength(0);
        for (Object o : new Object[] {A.A1, B.B2, B.B3, A.A2}) {
            sb.append(sb.length() > 0 ? "|" : "").append(enumQualified(o));
        }
        System.out.println("enum-qualified: " + sb);

        sb.setLength(0);
        for (Object o : new Object[] {new P(0, 0), new P(1, 2), new L(new P(0, 0), new P(3, 4)),
                new L(new P(1, 0), new P(3, 4)), "x"}) {
            sb.append(sb.length() > 0 ? "|" : "").append(nested(o));
        }
        System.out.println("nested: " + sb);

        try {
            System.out.println("match-exception: no exception " + bad(new Bad(1)));
        } catch (Throwable t) {
            Throwable c = t.getCause();
            System.out.println("match-exception: " + t.getClass().getName() + " cause="
                    + (c == null ? "null" : c.getClass().getName() + ": " + c.getMessage()));
        }
        System.out.println("match-exception-null-arm: " + bad(null));

        System.out.println("enum-switch: " + enumSwitch(N.ONE, true) + "|" + enumSwitch(N.TWO, true) + "|"
                + enumSwitch(N.THREE, true) + "|" + enumSwitch(N.THREE, false) + "|" + enumSwitch(null, true));

        float oddNan = Float.intBitsToFloat(0x7fc00001);
        System.out.println("rec-float: nan=" + new FF(Float.NaN).equals(new FF(oddNan)) + " zero="
                + new FF(0.0f).equals(new FF(-0.0f)) + " same=" + new FF(1.5f).equals(new FF(1.5f))
                + " hash-nan=" + (new FF(Float.NaN).hashCode() == new FF(oddNan).hashCode()));
        System.out.println("rec-double: nan=" + new DD(Double.NaN).equals(new DD(Double.longBitsToDouble(0x7ff8000000000001L)))
                + " zero=" + new DD(0.0).equals(new DD(-0.0)));

        int[] arr = {1, 2};
        A2 r1 = new A2(arr, "x");
        A2 r2 = new A2(arr, "x");
        A2 r3 = new A2(new int[] {1, 2}, "x");
        System.out.println("rec-array: same-array=" + r1.equals(r2) + " equal-content=" + r1.equals(r3)
                + " hash-same=" + (r1.hashCode() == r2.hashCode()));

        System.out.println("rec-toString: "
                + new F(Float.NaN, -0.0, 'x', (byte) -1, true, 9007199254740993L, null).toString().replace("L4W86SwitchAndRecordEdges$", ""));
        System.out.println("rec-array-toString: " + r1.toString().replaceAll("@[0-9a-f]+", "@H").replace("A2[", "A["));
        E e = new E();
        System.out.println("rec-empty: " + e + " hash=" + e.hashCode() + " equals=" + e.equals(new E()));
        O o1 = new O(new I(1e10), "n");
        System.out.println("rec-nested: " + o1 + " equals=" + o1.equals(new O(new I(1e10), "n")));
    }
}
