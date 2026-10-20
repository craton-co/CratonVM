// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 71, lane L7: the fused instance-field increment
// whose updated value is used, `aload_N; dup; getfield #F; dup_x1;
// <operand>; iadd | isub; [i2b | i2c | i2s]; putfield #F` (post-increment,
// the old value is left: `last = count++`, `a[size++] = x`) and `aload_N;
// dup; getfield #F; <operand>; iadd | isub; [i2b | i2c | i2s]; dup_x1;
// putfield #F` (pre-increment, the new value is left: `return ++count`,
// `s += (acc += k)`), with `dup2_x1` / `ladd` / `lsub` for a long field,
// under CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE (default off, on top of
// the default-on CRATONVM_INTERP_FUSED_FIELD_INCREMENT; the narrow and long
// lines are fused only with CRATONVM_INTERP_FUSED_FIELD_INCREMENT_WIDE as
// well, default on). Proposal
// i70-L7-proposal-fuse-the-field-increment-whose-value-is-used-20261103.
// No behaviour change: every line is the same with each switch off and on,
// with and without --nojit, with CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE
// and CRATONVM_INTERP_FUSED_FIELD_INCREMENT_NO_DUP, and with
// CRATONVM_JIT_NO_FIELD_FAST_PATH=1.
//
// Every value is checked where it is used (summed into a checksum or
// stored), so a hit that pushes the wrong value, the wrong width, or pushes
// nothing shows up as a different line.
//
// Shapes (each executed in a loop body, so the site is warm):
//   post int `last = post++`, `post--` used, wrapping at Integer.MAX_VALUE;
//   pre int `s += ++pre`, `s += (acc += k)` (iload), `s += (big -= 1000)`
//   (sipush); `return ++count` from a method; an append `buf[size++] = i`;
//   both forms on byte (wrap at 127), char (wrap at 0 / 65535) and short;
//   post and pre on long across the 32-bit carry and at Long.MAX_VALUE;
//   a value used deep in an expression (`f(1, 2, n++)`, `f(1, 2, ++n)`,
//   `x.pa++ + y.pa++` over two locals), a value feeding another field's
//   store (`this.a = this.bf++`: the group is the `bf` one);
//   declines at a warm site: a null receiver after 100 good ones, int and
//   long (the NPE is raised by the getfield, on the line of the use), a
//   receiver of a subclass at the same site; never fused: volatile int and
//   long (never quickened).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// post int last=999 post=1000 dec last=-999 post=-1000
// post wrap 2147483645 2147483646 2147483647 -2147483648
// pre int s=500500 acc=5000 s2=2502500 big=-50000 s3=-1275000
// pre method 5050 count=100
// append size=300 sum=44850 last=299
// byte post sum=-4132 b=44 pre sum=-4188 b2=44
// char post sum=2555179 c=65496 pre sum=2620665 c2=65496
// short post sum=196573 s=-32766 pre sum=131047 s2=-32766
// long post 4294967311 l=4294967312 pre 137438953488 l2=4294967312
// long top -9223372036854775808 pre top -9223372036854775808
// deep 1204950 1215050 n=200 xy=52450
// volatile int 499500 v=1000 long 4294967311 vl=4294967312
// two fields a=100 b=101
// npe int java.lang.NullPointerException after 100 at useInt:263
// npe long java.lang.NullPointerException after 100 at useLong:267
// poly int base=500 sub=500 used=249500 long base=500 sub=500
// HOTSPOT_EXPECTED_END
public class L7W71FusedFieldIncrementValue {
    int post;
    int top = Integer.MAX_VALUE - 2;
    int pre;
    int acc;
    int big;
    int count;
    int[] buf = new int[300];
    int size;
    byte b = 100;
    byte b2 = 100;
    char c = 10;
    char c2 = 10;
    short s = 32760;
    short s2 = 32760;
    long l = 0xFFFF_FFF0L;
    long l2 = 0xFFFF_FFF0L;
    long lTop = Long.MAX_VALUE;
    long lTop2 = Long.MAX_VALUE;
    int n;
    int a;
    int bf;
    volatile int v;
    volatile long vl = 0xFFFF_FFF0L;

    static class Holder {
        int hn;
        long hl;
    }

    static class Base {
        int bn;
        long bl;
    }

    static class Sub extends Base {
        int extra;
    }

    static class Pair {
        int pa;
    }

    void postInts() {
        int last = 0;
        for (int i = 0; i < 1000; i++) {
            last = post++;
        }
        System.out.print("post int last=" + last + " post=" + post);
        post = 0;
        for (int i = 0; i < 1000; i++) {
            last = post--;
        }
        System.out.println(" dec last=" + last + " post=" + post);
        StringBuilder sb = new StringBuilder("post wrap");
        for (int i = 0; i < 4; i++) {
            sb.append(' ').append(top++);
        }
        System.out.println(sb);
    }

    void preInts() {
        int sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += ++pre;
        }
        int k = 5;
        int sum2 = 0;
        for (int i = 0; i < 1000; i++) {
            sum2 += (acc += k);
        }
        int sum3 = 0;
        for (int i = 0; i < 50; i++) {
            sum3 += (big -= 1000);
        }
        System.out.println("pre int s=" + sum + " acc=" + acc + " s2=" + sum2 + " big=" + big
                + " s3=" + sum3);
    }

    int nextCount() {
        return ++count;
    }

    void preMethod() {
        int sum = 0;
        for (int i = 0; i < 100; i++) {
            sum += nextCount();
        }
        System.out.println("pre method " + sum + " count=" + count);
    }

    void append() {
        for (int i = 0; i < 300; i++) {
            buf[size++] = i;
        }
        int sum = 0;
        for (int x : buf) {
            sum += x;
        }
        System.out.println("append size=" + size + " sum=" + sum + " last=" + buf[299]);
    }

    void narrow() {
        int bsum = 0;
        int bsum2 = 0;
        for (int i = 0; i < 200; i++) {
            bsum += b++;
            bsum2 += ++b2;
        }
        System.out.println("byte post sum=" + bsum + " b=" + b + " pre sum=" + bsum2 + " b2=" + b2);
        int csum = 0;
        int csum2 = 0;
        for (int i = 0; i < 50; i++) {
            csum += c--;
            csum2 += --c2;
        }
        System.out.println("char post sum=" + csum + " c=" + (int) c + " pre sum=" + csum2
                + " c2=" + (int) c2);
        int ssum = 0;
        int ssum2 = 0;
        for (int i = 0; i < 10; i++) {
            ssum += s++;
            ssum2 += ++s2;
        }
        System.out.println("short post sum=" + ssum + " s=" + s + " pre sum=" + ssum2 + " s2=" + s2);
    }

    void longs() {
        long lsum = 0;
        long lsum2 = 0;
        for (int i = 0; i < 32; i++) {
            lsum = l++;
            lsum2 += ++l2;
        }
        System.out.println("long post " + lsum + " l=" + l + " pre " + lsum2 + " l2=" + l2);
        long t1 = lTop++;
        long t2 = ++lTop2;
        System.out.println("long top " + (t1 + 1) + " pre top " + t2);
    }

    static int f(int x, int y, int z) {
        return x * 10000 + y * 1000 + z;
    }

    void deep() {
        int sum = 0;
        for (int i = 0; i < 100; i++) {
            sum += f(1, 2, n++);
        }
        int sum2 = 0;
        for (int i = 0; i < 100; i++) {
            sum2 += f(1, 2, ++n);
        }
        Pair x = new Pair();
        Pair y = new Pair();
        y.pa = 1000;
        int xy = 0;
        for (int i = 0; i < 50; i++) {
            xy += x.pa++ + y.pa++;
        }
        System.out.println("deep " + sum + " " + sum2 + " n=" + n + " xy=" + xy);
    }

    void volatiles() {
        int sum = 0;
        for (int i = 0; i < 1000; i++) {
            sum += v++;
        }
        long last = 0;
        for (int i = 0; i < 32; i++) {
            last = vl++;
        }
        System.out.println("volatile int " + sum + " v=" + v + " long " + last + " vl=" + vl);
    }

    void twoFields() {
        bf = 1;
        for (int i = 0; i < 100; i++) {
            this.a = this.bf++;
        }
        System.out.println("two fields a=" + a + " b=" + bf);
    }

    static void npe(String what, boolean wide) {
        Holder good = new Holder();
        int done = 0;
        try {
            for (int i = 0; i <= 100; i++) {
                Holder o = i < 100 ? good : null;
                if (wide) {
                    useLong(o);
                } else {
                    useInt(o);
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
    static int useInt(Holder o) {
        return o.hn++;
    }

    static long useLong(Holder o) {
        return ++o.hl;
    }

    static int bumpBase(Base o) {
        o.bl++;
        return o.bn++;
    }

    public static void main(String[] args) {
        L7W71FusedFieldIncrementValue t = new L7W71FusedFieldIncrementValue();
        t.postInts();
        t.preInts();
        t.preMethod();
        t.append();
        t.narrow();
        t.longs();
        t.deep();
        t.volatiles();
        t.twoFields();
        npe("int", false);
        npe("long", true);
        Base base = new Base();
        Sub sub = new Sub();
        int used = 0;
        for (int i = 0; i < 1000; i++) {
            used += bumpBase((i & 1) == 0 ? base : sub);
        }
        System.out.println("poly int base=" + base.bn + " sub=" + sub.bn + " used=" + used
                + " long base=" + base.bl + " sub=" + sub.bl);
    }
}
