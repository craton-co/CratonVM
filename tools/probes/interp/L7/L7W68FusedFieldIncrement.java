// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L7: the fused instance-field int
// increment (`aload_N; dup; getfield #F; <int operand>; iadd | isub;
// putfield #F`; CRATONVM_INTERP_FUSED_FIELD_INCREMENT, default off; proposal
// docs/internal/fixed-bugs/interpreter-i67-L7-proposal-a-fused-instance-field-increment-DONE-20261031.md).
// No behaviour change: every line is the same with the switch off and on,
// with and without --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1
// (which disables the quickened field arms and so the fusion).
//
// Shapes (each executed in a loop body, so the site is warm):
//   fused: `count++` (aload_0; iconst_1; iadd), `down--` (isub), `plus += 7`
//   (bipush), `big += 1000` (sipush), `acc += i` (iload_1), `acc2 -= i`
//   with `i` past local 3 (iload n), a sum that wraps past
//   Integer.MAX_VALUE, `h.count++` through aload_0 of a static method and
//   aload_3, a field a callee reads after each increment, a field
//   incremented from the constructor, two threads each on its own object,
//   `ArrayList.add` (modCount++);
//   declines at a warm site: a null receiver after 100 good ones (the NPE is
//   raised by the getfield), a receiver of another class at the same site
//   (a subclass instance of the declaring class);
//   never fused (the unfused arms answer): `last = count3++` (a dup_x1 sits
//   between), `last = ++count4` (dup_x1 before the putfield), byte / short /
//   char fields (an i2b / i2s / i2c sits before the putfield), a long field,
//   a volatile field (never quickened).
//
// Not covered here: a JDWP / JVMTI FieldAccess or FieldModification watch
// armed mid-loop, and a breakpoint or single step inside the group (need a
// debugger driver; the helper declines on the same watch union the
// quickened getfield / putfield test, and the arm on `fusion_off`).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_FIELD_SITE=1): with
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT=1 the `fast-field: get hit` and
// `put hit` counts stay about the same (a fused group counts one of each, the
// hits it replaces), while the dispatch count drops.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// inc count 1000
// inc down -1000
// inc plus 7000
// inc big 100000
// inc acc 499500
// inc acc2 -499500
// inc wrap -2147483644
// inc observed 55
// inc ctor 300
// inc static-local 1000
// inc aload3 1000
// inc threads 2000
// inc list 1000
// npe warm java.lang.NullPointerException after 100
// inc poly base=500 sub=500
// inc post last=999 count3=1000
// inc pre last=1000 count4=1000
// inc byte -56
// inc short -25536
// inc char Z
// inc long 3000
// inc volatile 1000
// HOTSPOT_EXPECTED_END
import java.util.ArrayList;

public class L7W68FusedFieldIncrement {
    int count;
    int down;
    int plus;
    int big;
    int acc;
    int acc2;
    int wrap = Integer.MAX_VALUE - 5;
    int watched;
    int seen;
    int ctorCount;
    int count3;
    int count4;
    byte b;
    short s;
    char c = 'A';
    long l;
    volatile int v;

    static class Holder {
        int count;
    }

    static class Base {
        int n;
    }

    static class Sub extends Base {
        int extra;
    }

    L7W68FusedFieldIncrement() {
        for (int i = 0; i < 100; i++) {
            ctorCount += 3;
        }
    }

    void incLoop() {
        for (int i = 0; i < 1000; i++) {
            count++;
        }
    }

    void decLoop() {
        for (int i = 0; i < 1000; i++) {
            down--;
        }
    }

    void plusLoop() {
        for (int i = 0; i < 1000; i++) {
            plus += 7;
        }
    }

    void bigLoop() {
        for (int i = 0; i < 100; i++) {
            big += 1000;
        }
    }

    void accLoop() {
        for (int i = 0; i < 1000; i++) {
            acc += i;
        }
    }

    void acc2Loop() {
        int p = 0;
        int q = 0;
        int r = 0;
        for (int i = 0; i < 1000; i++) {
            acc2 -= i;
            p++;
        }
        q = p + r;
        if (q != 1000) {
            System.out.println("acc2 locals wrong " + q);
        }
    }

    void wrapLoop() {
        for (int i = 0; i < 10; i++) {
            wrap += 1;
        }
    }

    int readWatched() {
        return watched;
    }

    void observedLoop() {
        for (int i = 0; i < 10; i++) {
            watched++;
            seen += readWatched();
        }
    }

    void postLoop() {
        int last = 0;
        for (int i = 0; i < 1000; i++) {
            last = count3++;
        }
        System.out.println("inc post last=" + last + " count3=" + count3);
    }

    void preLoop() {
        int last = 0;
        for (int i = 0; i < 1000; i++) {
            last = ++count4;
        }
        System.out.println("inc pre last=" + last + " count4=" + count4);
    }

    void narrowLoops() {
        for (int i = 0; i < 200; i++) {
            b++;
        }
        for (int i = 0; i < 40; i++) {
            s += 1000;
        }
        for (int i = 0; i < 25; i++) {
            c++;
        }
        for (int i = 0; i < 1000; i++) {
            l += 3;
        }
        for (int i = 0; i < 1000; i++) {
            v++;
        }
    }

    static int staticLocal(Holder h) {
        for (int i = 0; i < 1000; i++) {
            h.count++;
        }
        return h.count;
    }

    static int aload3(int a, int b, int c, Holder h) {
        for (int i = 0; i < 1000; i++) {
            h.count++;
        }
        return h.count + a + b + c;
    }

    static void bump(Holder h) {
        h.count++;
    }

    static void bumpBase(Base o) {
        o.n++;
    }

    public static void main(String[] args) throws Exception {
        L7W68FusedFieldIncrement t = new L7W68FusedFieldIncrement();
        t.incLoop();
        System.out.println("inc count " + t.count);
        t.decLoop();
        System.out.println("inc down " + t.down);
        t.plusLoop();
        System.out.println("inc plus " + t.plus);
        t.bigLoop();
        System.out.println("inc big " + t.big);
        t.accLoop();
        System.out.println("inc acc " + t.acc);
        t.acc2Loop();
        System.out.println("inc acc2 " + t.acc2);
        t.wrapLoop();
        System.out.println("inc wrap " + t.wrap);
        t.observedLoop();
        System.out.println("inc observed " + t.seen);
        System.out.println("inc ctor " + t.ctorCount);
        System.out.println("inc static-local " + staticLocal(new Holder()));
        System.out.println("inc aload3 " + aload3(0, 0, 0, new Holder()));

        Holder h1 = new Holder();
        Holder h2 = new Holder();
        Thread th1 = new Thread(() -> staticLocal(h1));
        Thread th2 = new Thread(() -> staticLocal(h2));
        th1.start();
        th2.start();
        th1.join();
        th2.join();
        System.out.println("inc threads " + (h1.count + h2.count));

        ArrayList<Integer> list = new ArrayList<>();
        for (int i = 0; i < 1000; i++) {
            list.add(i);
        }
        System.out.println("inc list " + list.size());

        Holder good = new Holder();
        int done = 0;
        try {
            for (int i = 0; i <= 100; i++) {
                bump(i < 100 ? good : null);
                done++;
            }
            System.out.println("npe warm none");
        } catch (NullPointerException e) {
            System.out.println("npe warm " + e.getClass().getName() + " after " + done);
        }

        Base base = new Base();
        Sub sub = new Sub();
        for (int i = 0; i < 1000; i++) {
            bumpBase((i & 1) == 0 ? base : sub);
        }
        System.out.println("inc poly base=" + base.n + " sub=" + sub.n);

        t.postLoop();
        t.preLoop();
        t.narrowLoops();
        System.out.println("inc byte " + t.b);
        System.out.println("inc short " + t.s);
        System.out.println("inc char " + t.c);
        System.out.println("inc long " + t.l);
        System.out.println("inc volatile " + t.v);
    }
}
