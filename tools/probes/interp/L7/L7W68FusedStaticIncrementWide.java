// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 68, lane L7: the fused static increment for
// narrow and long statics (`getstatic #F (B/C/S); <int operand>; iadd |
// isub; i2b | i2c | i2s; putstatic #F` and `getstatic #F (J); lconst_0/1 |
// lload; ladd | lsub; putstatic #F`;
// CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE, default off, admitted only
// with the now-default CRATONVM_INTERP_FUSED_STATIC_INCREMENT; proposal
// docs/internal/fixed-bugs/interpreter-i67-L7-proposal-fused-static-increment-for-narrow-and-long-statics-DONE-20261031.md).
// No behaviour change: every line is the same with the switch off and on,
// with CRATONVM_INTERP_FUSED_STATIC_INCREMENT=0 (which disables both), and
// with and without --nojit.
//
// Shapes (each executed in a loop body, so the site is warm):
//   fused under the switch: `bb++` past 127, `bdown--` past -128, `ss +=
//   1000` past 32767, `ss2--` past -32768, `cc++`, `cc2--` past 0 (wraps to
//   65535), `seq++` (lconst_1; ladd), `seq2--` (lsub), `big += 1` past
//   Long.MAX_VALUE, a volatile long, `viaLocal += k` with `k` a long local
//   (lload_0) and with `k` past local 3 (lload n), `zero += 0` (lconst_0);
//   never fused (the unfused arms answer): `ldcSum += 1_000_000_007L` and
//   `two += 2` (ldc2_w operands), `lx = ly + 1` (two pool indices), an int
//   counter (the wave-67 shape, unchanged).
//
// Not covered here: a JDWP / JVMTI FieldAccess or FieldModification watch
// armed mid-loop, and a breakpoint or single step inside the group (need a
// debugger driver; the arm declines on the same watch union and on
// `fusion_off`, as every fusion does).
//
// Positive control (debug cratonvm, --nojit, CRATONVM_DBG_GETSTATIC_PROF=1):
// with the switch on, the static-site get-hit and put-hit counts stay the
// same (a fused group counts one of each).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// inc byte -56
// inc byte-down 56
// inc short -25536
// inc short-down 25536
// inc char Z
// inc char-down 65535
// inc long 1000
// inc long-down -1000
// inc long-wrap -9223372036854775806
// inc long-volatile 1000
// inc long-local 3000
// inc long-local4 -7000
// inc long-zero 5
// inc ldc 10000000070
// inc two 2000
// inc pair lx=1000 ly=999
// inc int 1000
// HOTSPOT_EXPECTED_END
public class L7W68FusedStaticIncrementWide {
    static byte bb;
    static byte bdown;
    static short ss;
    static short ss2;
    static char cc = 'A';
    static char cc2 = 'A';
    static long seq;
    static long seq2;
    static long big = Long.MAX_VALUE - 2;
    static volatile long vseq;
    static long viaLocal;
    static long viaLocal4;
    static long zero = 5;
    static long ldcSum;
    static long two;
    static long lx;
    static long ly;
    static int counter;

    static void narrow() {
        for (int i = 0; i < 200; i++) {
            bb++;
        }
        for (int i = 0; i < 200; i++) {
            bdown--;
        }
        for (int i = 0; i < 40; i++) {
            ss += 1000;
        }
        for (int i = 0; i < 40000; i++) {
            ss2--;
        }
        for (int i = 0; i < 25; i++) {
            cc++;
        }
        for (int i = 0; i < 66; i++) {
            cc2--;
        }
    }

    static void longs() {
        for (int i = 0; i < 1000; i++) {
            seq++;
        }
        for (int i = 0; i < 1000; i++) {
            seq2--;
        }
        for (int i = 0; i < 5; i++) {
            big += 1;
        }
        for (int i = 0; i < 1000; i++) {
            vseq++;
        }
        for (int i = 0; i < 1000; i++) {
            zero += 0L;
        }
    }

    static void localLong() {
        long k = 3;
        for (int i = 0; i < 1000; i++) {
            viaLocal += k;
        }
    }

    static void localLong4(int a, int b, long k) {
        // `k` is in slots 2-3 of a static method; `m` below in 4-5.
        long m = k + a + b;
        for (int i = 0; i < 1000; i++) {
            viaLocal4 -= m;
        }
    }

    static void unfused() {
        for (int i = 0; i < 10; i++) {
            ldcSum += 1_000_000_007L;
        }
        for (int i = 0; i < 1000; i++) {
            two += 2;
        }
        for (int i = 0; i < 1000; i++) {
            lx = ly + 1;
            ly = lx;
        }
        ly = lx - 1;
        for (int i = 0; i < 1000; i++) {
            counter++;
        }
    }

    public static void main(String[] args) {
        narrow();
        System.out.println("inc byte " + bb);
        System.out.println("inc byte-down " + bdown);
        System.out.println("inc short " + ss);
        System.out.println("inc short-down " + ss2);
        System.out.println("inc char " + cc);
        System.out.println("inc char-down " + (int) cc2);
        longs();
        System.out.println("inc long " + seq);
        System.out.println("inc long-down " + seq2);
        System.out.println("inc long-wrap " + big);
        System.out.println("inc long-volatile " + vseq);
        localLong();
        System.out.println("inc long-local " + viaLocal);
        localLong4(1, 2, 4L);
        System.out.println("inc long-local4 " + viaLocal4);
        System.out.println("inc long-zero " + zero);
        unfused();
        System.out.println("inc ldc " + ldcSum);
        System.out.println("inc two " + two);
        System.out.println("inc pair lx=" + lx + " ly=" + ly);
        System.out.println("inc int " + counter);
    }
}
