// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 70, lane L7: the fused instance-field increment
// written without `dup`, `aload_N; aload_N; getfield #F; <operand>; iadd |
// isub; [i2b | i2s | i2c]; putfield #F` (`count = count + 1`,
// `o.n = o.n - k`), under CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP
// (default off, on top of the default-on CRATONVM_INTERP_FUSED_FIELD_INCREMENT;
// the narrow and long lines are fused only with
// CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE as well). Proposal
// i69-L7-proposal-fuse-the-field-increment-without-dup-20261102.
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, with CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE
// and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1.
//
// Shapes (each executed in a loop body, so the site is warm):
//   fused: `count = count + 1` (iconst_1), wrapping past Integer.MAX_VALUE,
//   `down = down - 7` (bipush), `big = big + 1000` (sipush), `acc = acc + k`
//   and `acc2 = acc2 - i` (iload); under WIDE `b = (byte) (b + 1)` (i2b),
//   `s = (short) (s + 1000)` (i2s), `c = (char) (c + 1)` (i2c), `l = l + 1`
//   across the 32-bit carry and at Long.MAX_VALUE; through `o.n = o.n + 1`
//   with `o` a parameter (aload_0 of a static method) and aload_3; from the
//   constructor; two threads each on its own object; a field a callee reads
//   after each update;
//   declines at a warm site: a null receiver after 100 good ones, for an int
//   and a long field (the NPE is raised by the getfield, on the line of the
//   assignment), a receiver of a subclass at the same site;
//   never fused (the unfused arms answer): `a = b + 1` over two fields of
//   `this`, `this.cnt = other.cnt + 1` (two different locals), volatile int
//   and long fields (never quickened).
//
// Not covered here: a JDWP / JVMTI FieldAccess or FieldModification watch
// armed mid-loop, and a breakpoint or single step inside the group (need a
// debugger driver; the helper declines on the same watch union the quickened
// getfield / putfield test, and the arm on `fusion_off`, as for the `dup`
// form).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// int inc 1000
// int wrap -2147483646
// int bipush -700 sipush 50000
// int local-add 1000 local-sub -4950
// byte inc -56
// short plus -25536
// char inc Z
// long carry 4294967312
// long top -9223372036854775808
// volatile int 1000 long 4294967312
// ctor 100
// static-local 1000 aload3 300
// threads 2000
// observed 55
// two-fields a=42 b=41
// cross cnt=6 other=5
// npe int java.lang.NullPointerException after 100 at bumpInt:214
// npe long java.lang.NullPointerException after 100 at bumpLong:218
// poly int base=500 sub=500 long base=500 sub=500
// HOTSPOT_EXPECTED_END
public class L7W70FusedFieldIncrementNoDup {
    int count;
    int top = Integer.MAX_VALUE;
    int down;
    int big;
    int acc;
    int acc2;
    byte b;
    short s;
    char c = 'A';
    long l = 0xFFFF_FFF0L;
    long lTop = Long.MAX_VALUE;
    volatile int v;
    volatile long vl = 0xFFFF_FFF0L;
    int ctorN;
    int ol;
    int seen;
    int a;
    int bField = 41;
    int cnt;

    static class Holder {
        int n;
        long ln;
    }

    static class Base {
        int bn;
        long bl;
    }

    static class Sub extends Base {
        int extra;
    }

    L7W70FusedFieldIncrementNoDup() {
        for (int i = 0; i < 100; i++) {
            ctorN = ctorN + 1;
        }
    }

    void intLoops() {
        for (int i = 0; i < 1000; i++) {
            count = count + 1;
        }
        for (int i = 0; i < 3; i++) {
            top = top + 1;
        }
        for (int i = 0; i < 100; i++) {
            down = down - 7;
        }
        for (int i = 0; i < 50; i++) {
            big = big + 1000;
        }
        int k = 5;
        for (int i = 0; i < 200; i++) {
            acc = acc + k;
        }
        for (int i = 0; i < 100; i++) {
            acc2 = acc2 - i;
        }
    }

    void narrowLoops() {
        for (int i = 0; i < 200; i++) {
            b = (byte) (b + 1);
        }
        for (int i = 0; i < 40; i++) {
            s = (short) (s + 1000);
        }
        for (int i = 0; i < 25; i++) {
            c = (char) (c + 1);
        }
    }

    void longLoops() {
        for (int i = 0; i < 32; i++) {
            l = l + 1;
        }
        lTop = lTop + 1;
    }

    void volatileLoops() {
        for (int i = 0; i < 1000; i++) {
            v = v + 1;
        }
        for (int i = 0; i < 32; i++) {
            vl = vl + 1;
        }
    }

    int readOl() {
        return ol;
    }

    void observedLoop() {
        for (int i = 0; i < 10; i++) {
            ol = ol + 1;
            seen = seen + readOl();
        }
    }

    void twoFields() {
        for (int i = 0; i < 10; i++) {
            a = bField + 1;
        }
    }

    void cross(L7W70FusedFieldIncrementNoDup other) {
        for (int i = 0; i < 10; i++) {
            this.cnt = other.cnt + 1;
        }
    }

    static void staticLocal(Holder o) {
        for (int i = 0; i < 1000; i++) {
            o.n = o.n + 1;
        }
    }

    static int aload3(int x, int y, int z, Holder h) {
        for (int i = 0; i < 300; i++) {
            h.n = h.n + 1;
        }
        return x + y + z;
    }

    static void bumpBase(Base o) {
        o.bn = o.bn + 1;
        o.bl = o.bl + 1;
    }

    static void npe(String what, boolean wide) {
        Holder good = new Holder();
        int done = 0;
        try {
            for (int i = 0; i <= 100; i++) {
                Holder o = i < 100 ? good : null;
                if (wide) {
                    bumpLong(o);
                } else {
                    bumpInt(o);
                }
                done++;
            }
            System.out.println("npe " + what + " none");
        } catch (NullPointerException e) {
            StackTraceElement frame = e.getStackTrace()[0];
            System.out.println("npe " + what + " " + e.getClass().getName() + " after " + done
                    + " at " + frame.getMethodName() + ":" + frame.getLineNumber());
        }
    }

    // Keep these two at the lines the expected block names.
    static void bumpInt(Holder o) {
        o.n = o.n + 1;
    }

    static void bumpLong(Holder o) {
        o.ln = o.ln + 1;
    }

    public static void main(String[] args) throws Exception {
        L7W70FusedFieldIncrementNoDup t = new L7W70FusedFieldIncrementNoDup();
        t.intLoops();
        System.out.println("int inc " + t.count);
        System.out.println("int wrap " + t.top);
        System.out.println("int bipush " + t.down + " sipush " + t.big);
        System.out.println("int local-add " + t.acc + " local-sub " + t.acc2);
        t.narrowLoops();
        System.out.println("byte inc " + t.b);
        System.out.println("short plus " + t.s);
        System.out.println("char inc " + t.c);
        t.longLoops();
        System.out.println("long carry " + t.l);
        System.out.println("long top " + t.lTop);
        t.volatileLoops();
        System.out.println("volatile int " + t.v + " long " + t.vl);
        System.out.println("ctor " + t.ctorN);

        Holder sl = new Holder();
        staticLocal(sl);
        Holder a3 = new Holder();
        aload3(0, 0, 0, a3);
        System.out.println("static-local " + sl.n + " aload3 " + a3.n);

        Holder h1 = new Holder();
        Holder h2 = new Holder();
        Thread th1 = new Thread(() -> staticLocal(h1));
        Thread th2 = new Thread(() -> staticLocal(h2));
        th1.start();
        th2.start();
        th1.join();
        th2.join();
        System.out.println("threads " + (h1.n + h2.n));

        t.observedLoop();
        System.out.println("observed " + t.seen);

        t.twoFields();
        System.out.println("two-fields a=" + t.a + " b=" + t.bField);
        L7W70FusedFieldIncrementNoDup other = new L7W70FusedFieldIncrementNoDup();
        other.cnt = 5;
        t.cross(other);
        System.out.println("cross cnt=" + t.cnt + " other=" + other.cnt);

        npe("int", false);
        npe("long", true);

        Base base = new Base();
        Sub sub = new Sub();
        for (int i = 0; i < 1000; i++) {
            bumpBase((i & 1) == 0 ? base : sub);
        }
        System.out.println("poly int base=" + base.bn + " sub=" + sub.bn
                + " long base=" + base.bl + " sub=" + sub.bl);
    }
}
