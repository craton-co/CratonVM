// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 2, lane fuzz: HOIST13-2, receiver-field accumulators in
// METHOD-ENTRY bodies (`CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY`,
// `loop_field_forward::promote_method_entry_accumulators`, the
// `receiver_non_null` rule of `forward_loop`).
//
// Risk targeted:
//   * ALIASING through a second base: `this.f += x; other.f += y` with
//     `other == this` at run time -- the forward must treat the other base's
//     store as a write of the same cell;
//   * FIELD SHADOWING: `Sub` declares its own `f`; `Base.f` and `Sub.f` are
//     different cells of one object, accumulated in one loop;
//   * an INHERITED field accumulated by a method of the subclass, with two
//     receiver classes reaching the same compiled body;
//   * a long cell and an int cell of `this` in one loop, plus a read of a
//     third field the loop never writes;
//   * the field read through a CALL in the loop (a non-inlined virtual call
//     that reads `this.f`): the loop must not be promoted, or the callee sees
//     stale memory;
//   * nested loops: the inner accumulates `this.f`, the outer reads it.
// Rows cold, hot (the methods called 50k+ times), after a deopt trigger
// (a late subclass overriding the call target), and again.
//
// Run:
//   javac -d out R21FzLoopField1.java
//   java -Xmx256m -cp out R21FzLoopField1                                 # HotSpot 25
//   cratonvm -Xmx256m -cp out R21FzLoopField1
//   CRATONVM_JIT_IR_LOOP_FIELD_PROMOTE_METHOD_ENTRY=1 cratonvm -Xmx256m -cp out R21FzLoopField1
//   cratonvm --compatible ..., cratonvm --nojit ...
// HotSpot expectation: every line identical on every arm.

public class R21FzLoopField1 {
    static final int WARM = 60_000;

    static class Base {
        int f;
        long g;
        int ro = 3;

        int aliasAdd(Base other, int n) {
            for (int i = 0; i < n; i++) {
                f += i;
                other.f += 2;
            }
            return f;
        }

        long twoCells(int n) {
            for (int i = 0; i < n; i++) {
                f += ro;
                g += (long) f * i;
            }
            return g + f;
        }

        int peek() {
            return f;
        }

        int withCall(int n) {
            int s = 0;
            for (int i = 0; i < n; i++) {
                f += 1;
                s += peek();
            }
            return s;
        }

        int nested(int outer, int inner) {
            int t = 0;
            for (int o = 0; o < outer; o++) {
                for (int i = 0; i < inner; i++) {
                    f += i + o;
                }
                t = t * 7 + f;
            }
            return t;
        }
    }

    static class Sub extends Base {
        int f; // shadows Base.f

        int shadow(int n) {
            for (int i = 0; i < n; i++) {
                f += i;
                super.f -= 1;
            }
            return f * 1000 + super.f;
        }

        int inherited(int n) {
            for (int i = 0; i < n; i++) {
                ((Base) this).f += 5;
            }
            return ((Base) this).f;
        }
    }

    static final class Sub2 extends Sub {
        int pad;
    }

    static final class Late extends Base {
        @Override
        int peek() {
            return -f;
        }
    }

    static boolean lateLoaded;

    static void rows(String tag) {
        Base b = new Base();
        Base c = new Base();
        System.out.println("row " + tag + " aliasSelf " + b.aliasAdd(b, 17) + " " + b.f);
        b = new Base();
        System.out.println("row " + tag + " aliasOther " + b.aliasAdd(c, 17) + " " + b.f + " " + c.f);
        b = new Base();
        System.out.println("row " + tag + " twoCells " + b.twoCells(29) + " " + b.f + " " + b.g);
        b = new Base();
        System.out.println("row " + tag + " withCall " + b.withCall(13) + " " + b.f);
        if (lateLoaded) {
            b = new Late();
            System.out.println("row " + tag + " withCall-late " + b.withCall(13) + " " + b.f);
        }
        b = new Base();
        System.out.println("row " + tag + " nested " + b.nested(6, 11) + " " + b.f);
        Sub s = new Sub();
        System.out.println("row " + tag + " shadow " + s.shadow(19) + " " + s.f + " " + ((Base) s).f);
        Sub s2 = new Sub2();
        System.out.println("row " + tag + " shadow2 " + s2.shadow(19) + " " + s2.f + " " + ((Base) s2).f);
        System.out.println("row " + tag + " inherited " + s.inherited(9) + " " + s2.inherited(9));
    }

    public static void main(String[] args) {
        rows("cold");
        long h = 0;
        Base b = new Base();
        Base c = new Base();
        Sub s = new Sub();
        Sub s2 = new Sub2();
        for (int r = 0; r < WARM; r++) {
            int n = 4 + (r & 15);
            h += b.aliasAdd((r & 1) == 0 ? b : c, n);
            h += b.twoCells(n);
            h += b.withCall(n);
            h += b.nested(2, n);
            h += s.shadow(n);
            h += s2.shadow(n);
            h += ((r & 1) == 0 ? s : s2).inherited(n);
            h = h * 31 + b.f + c.f;
        }
        System.out.println("warm hash " + h + " " + b.f + " " + b.g + " " + c.f + " " + s.f + " " + s2.f);
        rows("hot");
        lateLoaded = true;
        Base late = new Late();
        System.out.println("late " + late.withCall(100) + " " + late.f);
        rows("after");
    }
}
