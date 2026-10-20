// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L7: the cast-site hit reads the memo in
// the table entry instead of copying the site (`CRATONVM_CAST_HIT_IN_PLACE`,
// `opcodes::cast_site_hit_in_place`; proposal
// `interpreter-L7-proposal-the-cast-site-hit-reads-the-memo-in-place-FIXED-20261022`). A
// performance switch: every line must be the same with it on and off. Each
// shape runs cold (r0) and warm (r1..r3), and the monomorphic row runs again
// after a class definition moves the definition epoch (`late`), so the
// in-place hit must decline a memo proved under the old epoch:
//
//   mono    `checkcast Base` / `instanceof Base` of one receiver class
//   bi      the same sites with a second receiver class (the older slot)
//   neg     `instanceof Base` refusing two unrelated classes (negative slots;
//           `checkcast` of one of them throws)
//   arr     `instanceof Object[]` / `(Object[])` of a String[] and an int[]
//           (array receivers take the copying protocol)
//   lam     `(Runnable)` / `instanceof Runnable` of a lambda, then a Supplier
//   late    `mono` again after a class nobody loaded before is defined
//   loop    300000 executions of the mono and bi sites
//
// Positive control: `CRATONVM_DBG_FIELD_SITE=1`, FINAL `[site-cache]` line,
// `cast-in-place-hit: N` with N in the hundreds of thousands with
// `CRATONVM_CAST_HIT_IN_PLACE=1` (or default on after the lane's switch-on
// commit), and `cast-in-place-hit: 0` with `CRATONVM_CAST_HIT_IN_PLACE=0`;
// `cast: hit=` must stay of the same order either way.
// `CRATONVM_DBG_CAST_MEMO_CROSSCHECK=1` must print no `[CAST-MEMO-CROSSCHECK]`
// line.
//
// Expected (HotSpot 25.0.3, `java L7W56CastHitInPlace`, and `-Xint`):
//   r0 mono 1 true 1 true
//   r0 bi 2 true 1 true
//   r0 neg false false true java.lang.ClassCastException
//   r0 arr true false 1 java.lang.ClassCastException
//   r0 lam 1 true false java.lang.ClassCastException
//   r1 mono 1 true 1 true
//   r1 bi 2 true 1 true
//   r1 neg false false true java.lang.ClassCastException
//   r1 arr true false 1 java.lang.ClassCastException
//   r1 lam 1 true false java.lang.ClassCastException
//   r2 mono 1 true 1 true
//   r2 bi 2 true 1 true
//   r2 neg false false true java.lang.ClassCastException
//   r2 arr true false 1 java.lang.ClassCastException
//   r2 lam 1 true false java.lang.ClassCastException
//   r3 mono 1 true 1 true
//   r3 bi 2 true 1 true
//   r3 neg false false true java.lang.ClassCastException
//   r3 arr true false 1 java.lang.ClassCastException
//   r3 lam 1 true false java.lang.ClassCastException
//   late 7 mono 1 true 1 true
//   loop 300000 300000 300000 300000
import java.util.function.Supplier;

public class L7W56CastHitInPlace {
    static class Base { int v() { return 0; } }
    static final class SubA extends Base { int v() { return 1; } }
    static final class SubB extends Base { int v() { return 2; } }
    static final class Other {}
    static final class Unrelated {}
    // Loaded only for `late`: its definition moves the definition epoch.
    static final class Late { int seven() { return 7; } }

    // One site each: every call below reaches the same bytecode.
    static int castBase(Object o) {
        return ((Base) o).v();
    }

    static boolean isBase(Object o) {
        return o instanceof Base;
    }

    static boolean isObjects(Object o) {
        return o instanceof Object[];
    }

    static int castObjects(Object o) {
        return ((Object[]) o).length;
    }

    static Runnable castRunnable(Object o) {
        return (Runnable) o;
    }

    static boolean isRunnable(Object o) {
        return o instanceof Runnable;
    }

    static String cce(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (ClassCastException e) {
            return e.getClass().getName();
        }
    }

    static String mono(Object a) {
        int va = castBase(a);
        boolean ia = isBase(a);
        int va2 = castBase(a);
        boolean ia2 = isBase(a);
        return "mono " + va + " " + ia + " " + va2 + " " + ia2;
    }

    static void round(int r) {
        Object a = new SubA();
        Object b = new SubB();
        Object other = new Other();
        Object unrelated = new Unrelated();
        System.out.println("r" + r + " " + mono(a));

        int vb = castBase(b);
        boolean ib = isBase(b);
        int va = castBase(a);
        boolean ia = isBase(a);
        System.out.println("r" + r + " bi " + vb + " " + ib + " " + va + " " + ia);

        boolean no1 = isBase(other);
        boolean no2 = isBase(unrelated);
        boolean yes = isBase(a);
        String neg = cce(() -> castBase(other));
        System.out.println("r" + r + " neg " + no1 + " " + no2 + " " + yes + " " + neg);

        Object strings = new String[] {"x"};
        Object ints = new int[1];
        boolean arrYes = isObjects(strings);
        boolean arrNo = isObjects(ints);
        int len = castObjects(strings);
        String arr = cce(() -> castObjects(ints));
        System.out.println("r" + r + " arr " + arrYes + " " + arrNo + " " + len + " " + arr);

        int[] hits = new int[1];
        Runnable l1 = () -> hits[0] += 1;
        Supplier<String> sup = () -> "s";
        castRunnable(l1).run();
        boolean lr = isRunnable(l1);
        boolean ls = isRunnable(sup);
        String lam = cce(() -> castRunnable(sup));
        System.out.println("r" + r + " lam " + hits[0] + " " + lr + " " + ls + " " + lam);
    }

    public static void main(String[] args) {
        for (int r = 0; r < 4; r++) {
            round(r);
        }
        int seven = new Late().seven();
        System.out.println("late " + seven + " " + mono(new SubA()));
        Object a = new SubA();
        Object b = new SubB();
        long monoCast = 0;
        long monoIs = 0;
        long biCast = 0;
        long biIs = 0;
        for (int i = 0; i < 300_000; i++) {
            monoCast += castBase(a) > 0 ? 1 : 0;
            monoIs += isBase(a) ? 1 : 0;
            biCast += castBase((i & 1) == 0 ? a : b) > 0 ? 1 : 0;
            biIs += isBase((i & 1) == 0 ? b : a) ? 1 : 0;
        }
        System.out.println("loop " + monoCast + " " + monoIs + " " + biCast + " " + biIs);
    }
}
