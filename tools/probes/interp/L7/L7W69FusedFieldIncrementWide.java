// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 69, lane L7: the fused instance-field increment
// widened to narrow and long fields (CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE,
// default off, on top of the default-on CRATONVM_INTERP_FUSED_FIELD_INCREMENT)
// and the byte-only early decline of both fused increments
// (CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE, default off). Proposals
// i68-L7-proposal-fused-field-increment-for-narrow-and-long-fields-20261101 and
// i68-L7-proposal-decline-the-fused-increments-before-the-call-20261101.
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, and with CRATONVM_JIT_NO_FIELD_FAST_PATH=1.
//
// Shapes (each executed in a loop body, so the site is warm):
//   fused under WIDE: `b++` / `b--` / `b += 100` / `b -= k` on a byte field
//   (i2b), `s += 1000` / `s--` / `s += 30000` (sipush) on a short (i2s),
//   `c++` / `c--` / `c += 1000` on a char (i2c), each wrapping past its
//   range; `l++` across the 32-bit carry, `l++` at Long.MAX_VALUE, `l--` at
//   Long.MIN_VALUE, `l += k` / `l -= k` with a long local (lload), `l += 0L`
//   (lconst_0); through aload_0 of a static method and aload_3; from the
//   constructor; two threads each on its own object; a field a callee reads
//   after each increment;
//   declines at a warm site: a null receiver after 100 good ones, for a byte
//   and a long field (the NPE is raised by the getfield, on the line of the
//   increment), a receiver of a subclass at the same site;
//   never fused (the unfused arms answer): `l += i` with an int `i` (an i2l
//   sits before the ladd), `l += 3` (ldc2_w), `last = pb++` on a byte
//   (dup_x1), `last = ++pl` on a long (dup2_x1), volatile byte / short / char
//   / long fields (never quickened).
//
// Not covered here: a JDWP / JVMTI FieldAccess or FieldModification watch
// armed mid-loop, and a breakpoint or single step inside the group (need a
// debugger driver; the helper declines on the same watch union the quickened
// getfield / putfield test, and the arm on `fusion_off`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// byte inc -56
// byte dec 125
// byte plus 44
// byte sub 116
// short plus -25536
// short dec 32766
// short sipush 24464
// char inc Z
// char dec 65534
// char plus 4464
// long carry 4294967312
// long top -9223372036854775808
// long bottom 9223372036854775807
// long local-add 3000
// long local-sub -12884901888
// long zero 5
// long int-local 499500
// long ldc2w 3000
// volatile byte -56 short -25536 char Z long 4294967312
// ctor long 100 byte 100
// static-local long 1000 short -25536
// aload3 byte 44 long 300
// threads long 2000 short 2000
// observed 55
// npe byte java.lang.NullPointerException after 100 at bumpByte:253
// npe long java.lang.NullPointerException after 100 at bumpLong:257
// poly byte base=-12 sub=-12 long base=500 sub=500
// post last=-57 pb=-56
// pre last=1000 pl=1000
// HOTSPOT_EXPECTED_END
public class L7W69FusedFieldIncrementWide {
    byte b;
    byte bDown = -128;
    byte bPlus;
    byte bSub;
    short s;
    short sDown = Short.MIN_VALUE;
    short sBig;
    char c = 'A';
    char cDown;
    char cPlus;
    long l = 0xFFFF_FFF0L;
    long lTop = Long.MAX_VALUE;
    long lBottom = Long.MIN_VALUE;
    long lAdd;
    long lSub;
    long lZero = 5;
    long lInt;
    long lLdc;
    volatile byte vb;
    volatile short vs;
    volatile char vc = 'A';
    volatile long vl = 0xFFFF_FFF0L;
    long ctorL;
    byte ctorB;
    long ol;
    long seen;
    byte pb;
    long pl;

    static class Holder {
        byte hb;
        short hs;
        long hl;
    }

    static class Base {
        byte bb;
        long bl;
    }

    static class Sub extends Base {
        int extra;
    }

    L7W69FusedFieldIncrementWide() {
        for (int i = 0; i < 100; i++) {
            ctorL++;
            ctorB++;
        }
    }

    void byteLoops() {
        for (int i = 0; i < 200; i++) {
            b++;
        }
        for (int i = 0; i < 3; i++) {
            bDown--;
        }
        for (int i = 0; i < 3; i++) {
            bPlus += 100;
        }
        int k = 7;
        for (int i = 0; i < 20; i++) {
            bSub -= k;
        }
    }

    void shortLoops() {
        for (int i = 0; i < 40; i++) {
            s += 1000;
        }
        for (int i = 0; i < 2; i++) {
            sDown--;
        }
        for (int i = 0; i < 3; i++) {
            sBig += 30000;
        }
    }

    void charLoops() {
        for (int i = 0; i < 25; i++) {
            c++;
        }
        for (int i = 0; i < 2; i++) {
            cDown--;
        }
        for (int i = 0; i < 70; i++) {
            cPlus += 1000;
        }
    }

    void longLoops() {
        for (int i = 0; i < 32; i++) {
            l++;
        }
        lTop++;
        lBottom--;
        long k = 3;
        for (int i = 0; i < 1000; i++) {
            lAdd += k;
        }
        long big = 0x1_0000_0000L;
        for (int i = 0; i < 3; i++) {
            lSub -= big;
        }
        for (int i = 0; i < 10; i++) {
            lZero += 0L;
        }
        for (int i = 0; i < 1000; i++) {
            lInt += i;
        }
        for (int i = 0; i < 1000; i++) {
            lLdc += 3;
        }
    }

    void volatileLoops() {
        for (int i = 0; i < 200; i++) {
            vb++;
        }
        for (int i = 0; i < 40; i++) {
            vs += 1000;
        }
        for (int i = 0; i < 25; i++) {
            vc++;
        }
        for (int i = 0; i < 32; i++) {
            vl++;
        }
    }

    long readOl() {
        return ol;
    }

    void observedLoop() {
        for (int i = 0; i < 10; i++) {
            ol++;
            seen += readOl();
        }
    }

    void postLoop() {
        int last = 0;
        for (int i = 0; i < 200; i++) {
            last = pb++;
        }
        System.out.println("post last=" + last + " pb=" + pb);
    }

    void preLoop() {
        long last = 0;
        for (int i = 0; i < 1000; i++) {
            last = ++pl;
        }
        System.out.println("pre last=" + last + " pl=" + pl);
    }

    static void staticLocal(Holder h) {
        for (int i = 0; i < 1000; i++) {
            h.hl++;
            h.hs++;
        }
    }

    static void staticLocalShort(Holder h) {
        for (int i = 0; i < 40; i++) {
            h.hs += 1000;
        }
    }

    static long aload3(int a, int b, int c, Holder h) {
        for (int i = 0; i < 300; i++) {
            h.hb++;
            h.hl++;
        }
        return a + b + c;
    }

    static void bumpBase(Base o) {
        o.bb++;
        o.bl++;
    }

    static void bumpByte(Holder o) {
        o.hb++;
    }

    static void bumpLong(Holder o) {
        o.hl++;
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
                    bumpByte(o);
                }
                done++;
            }
            System.out.println("npe " + what + " none");
        } catch (NullPointerException e) {
            StackTraceElement top = e.getStackTrace()[0];
            System.out.println("npe " + what + " " + e.getClass().getName() + " after " + done
                    + " at " + top.getMethodName() + ":" + top.getLineNumber());
        }
    }

    public static void main(String[] args) throws Exception {
        L7W69FusedFieldIncrementWide t = new L7W69FusedFieldIncrementWide();
        t.byteLoops();
        System.out.println("byte inc " + t.b);
        System.out.println("byte dec " + t.bDown);
        System.out.println("byte plus " + t.bPlus);
        System.out.println("byte sub " + t.bSub);
        t.shortLoops();
        System.out.println("short plus " + t.s);
        System.out.println("short dec " + t.sDown);
        System.out.println("short sipush " + t.sBig);
        t.charLoops();
        System.out.println("char inc " + t.c);
        System.out.println("char dec " + (int) t.cDown);
        System.out.println("char plus " + (int) t.cPlus);
        t.longLoops();
        System.out.println("long carry " + t.l);
        System.out.println("long top " + t.lTop);
        System.out.println("long bottom " + t.lBottom);
        System.out.println("long local-add " + t.lAdd);
        System.out.println("long local-sub " + t.lSub);
        System.out.println("long zero " + t.lZero);
        System.out.println("long int-local " + t.lInt);
        System.out.println("long ldc2w " + t.lLdc);
        t.volatileLoops();
        System.out.println("volatile byte " + t.vb + " short " + t.vs + " char " + t.vc
                + " long " + t.vl);
        System.out.println("ctor long " + t.ctorL + " byte " + t.ctorB);

        Holder sl = new Holder();
        staticLocal(sl);
        Holder ss = new Holder();
        staticLocalShort(ss);
        System.out.println("static-local long " + sl.hl + " short " + ss.hs);
        Holder a3 = new Holder();
        aload3(0, 0, 0, a3);
        System.out.println("aload3 byte " + a3.hb + " long " + a3.hl);

        Holder h1 = new Holder();
        Holder h2 = new Holder();
        Thread th1 = new Thread(() -> staticLocal(h1));
        Thread th2 = new Thread(() -> staticLocal(h2));
        th1.start();
        th2.start();
        th1.join();
        th2.join();
        System.out.println("threads long " + (h1.hl + h2.hl) + " short " + (h1.hs + h2.hs));

        t.observedLoop();
        System.out.println("observed " + t.seen);

        npe("byte", false);
        npe("long", true);

        Base base = new Base();
        Sub sub = new Sub();
        for (int i = 0; i < 1000; i++) {
            bumpBase((i & 1) == 0 ? base : sub);
        }
        System.out.println("poly byte base=" + base.bb + " sub=" + sub.bb
                + " long base=" + base.bl + " sub=" + sub.bl);

        t.postLoop();
        t.preLoop();
    }
}
