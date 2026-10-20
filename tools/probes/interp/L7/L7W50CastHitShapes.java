// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 50, lane L7: the cast-site hit validates its
// receiver with `VmHeap::is_object_start` (no `ObjectRef` built), and a
// `checkcast` of null leaves the operand where it peeked it (no pop/push).
// Every shape the hit answers, cold (r0) and warm (r1..r2), at one site per
// shape, and the null cast in between (a null must not disturb the stack
// under it: the `pair` row casts a null above a live value).
//
//   class      (Base) on a subclass, then instanceof
//   iface      (Shape) on an implementor, then instanceof
//   poly       two receiver classes through one site (memo refill)
//   array      (Object[]) on a String[] (array memo), instanceof String[]
//   null       (Base) null, instanceof null
//   pair       a null cast with a live value under it
//   refuse     (Base) on an unrelated class: ClassCastException
//
// Expected (HotSpot 25.0.3, `java L7W50CastHitShapes`, and `-Xint`):
//   r0 class 1 true
//   r0 iface 1 true
//   r0 poly 2 true true
//   r0 array 2 true
//   r0 null null false
//   r0 pair null 7
//   r0 refuse ClassCastException
//   r1 class 1 true
//   r1 iface 1 true
//   r1 poly 2 true true
//   r1 array 2 true
//   r1 null null false
//   r1 pair null 7
//   r1 refuse ClassCastException
//   r2 class 1 true
//   r2 iface 1 true
//   r2 poly 2 true true
//   r2 array 2 true
//   r2 null null false
//   r2 pair null 7
//   r2 refuse ClassCastException
public class L7W50CastHitShapes {
    static class Base {
        int v() {
            return 1;
        }
    }

    static final class SubA extends Base {}

    static final class SubB extends Base {}

    interface Shape {
        int size();
    }

    static final class Sq implements Shape {
        public int size() {
            return 1;
        }
    }

    static final class Other {}

    static Object pick(Object o) {
        return o;
    }

    static String classRow(Object o) {
        return ((Base) o).v() + " " + (o instanceof Base);
    }

    static String ifaceRow(Object o) {
        return ((Shape) o).size() + " " + (o instanceof Shape);
    }

    static int polyCast(Object o) {
        return ((Base) o).v();
    }

    static boolean polyTest(Object o) {
        return o instanceof Base;
    }

    static String arrayRow(Object o) {
        return ((Object[]) o).length + " " + (o instanceof String[]);
    }

    static String nullRow(Object o) {
        Base b = (Base) o;
        return b + " " + (o instanceof Base);
    }

    static String pairRow(Object under, Object o) {
        return pairOf((Integer) under, (Base) o);
    }

    static String pairOf(Integer under, Base b) {
        return b + " " + under;
    }

    static String refuseRow(Object o) {
        try {
            return "cast " + ((Base) o).v();
        } catch (ClassCastException e) {
            return e.getClass().getSimpleName();
        }
    }

    public static void main(String[] args) {
        for (int r = 0; r < 3; r++) {
            String p = "r" + r + " ";
            System.out.println(p + "class " + classRow(pick(new SubA())));
            System.out.println(p + "iface " + ifaceRow(pick(new Sq())));
            int sum = polyCast(pick(new SubA())) + polyCast(pick(new SubB()));
            System.out.println(p + "poly " + sum + " " + polyTest(pick(new SubA()))
                    + " " + polyTest(pick(new SubB())));
            System.out.println(p + "array " + arrayRow(pick(new String[] {"a", "b"})));
            System.out.println(p + "null " + nullRow(pick(null)));
            System.out.println(p + "pair " + pairRow(pick(7), pick(null)));
            System.out.println(p + "refuse " + refuseRow(pick(new Other())));
        }
    }
}
