// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 15, lane fuzz4: VarHandle access modes on every field
// kind, array elements and byte-array views, compiled hot and deoptimized.
//
// Risk targeted: the VarHandle constant folding and direct helpers
// (`CRATONVM_JIT_IR_VH_FIELD_FOLD`, `CRATONVM_JIT_IR_VH_CONST_ADMIT`,
// `CRATONVM_JIT_IR_VH_READ_BINDS`, `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT`,
// `CRATONVM_JIT_VARHANDLE_{READ,WRITE,CAS}_DIRECT_HELPERS`,
// `CRATONVM_JIT_VARHANDLE_BYTE_ARRAY_VIEW_READ`, `CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT`)
// and the unsafe field-cell arms they lower to
// (`CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE`, `..._CAE_INLINE`, `..._BITWISE_INLINE`,
// `CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS`). Facts:
//   * sub-word RMW wrap: getAndAdd on byte 127, short 0x7fff, char 0xffff;
//     getAndBitwiseXor on boolean; the stored neighbour fields must not be
//     clobbered by a wider store (fields are declared so that byte, short,
//     char and boolean pack next to each other);
//   * float / double CAS compare BITWISE: CAS(NaN -> 1) succeeds on a NaN
//     with the same bits, CAS(0.0 -> 1) fails on -0.0, getAndAdd on float;
//   * compareAndExchange witness values, weakCompareAndSetPlain retried until
//     it succeeds (spurious failure is legal), getAndSet on references;
//   * unsupported modes (getAndAdd on boolean / Object, bitwise on float)
//     throw UnsupportedOperationException;
//   * array element handles: index -1 and length (AIOOBE), a wrong-typed
//     store through an Object[] handle into a String[] (ArrayStoreException),
//     a null array (NPE);
//   * byteArrayViewVarHandle int/long, LITTLE and BIG endian, unaligned plain
//     get/set (allowed), the set of supported view access modes (printed as a
//     fact: HotSpot 25 refuses getAndAdd with UnsupportedOperationException),
//     unaligned getVolatile / getAndAdd (IllegalStateException or UOE,
//     "Misaligned access at index: N"), index past the end (IOOBE);
//   * a handle invoked with a receiver of the wrong class (ClassCastException)
//     or null (NullPointerException);
//   * a static-field handle whose holder class is initialized by the first
//     access (the <clinit> side effect is printed).
// Hot kernels run every mode 100k times through static final handles (the
// constant-folded path) and through a handle read from a non-final array (the
// generic path), then a never-loaded subclass of the receiver class arrives
// (receiver type profile change, deopt), then again.
//
// Run:
//   javac -d out R21Fz4VhModes1.java
//   java -Xmx256m -cp out R21Fz4VhModes1                     # HotSpot 25
//   cratonvm -Xmx256m -cp out R21Fz4VhModes1                 # --jdk-only
//   cratonvm --compatible -Xmx256m -cp out R21Fz4VhModes1
//   cratonvm --nojit -Xmx256m -cp out R21Fz4VhModes1
//   CRATONVM_JIT_UNSAFE_FIELD_CELLS_BYTE_OFFSETS=1 cratonvm -Xmx256m -cp out R21Fz4VhModes1
//   CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT=1 CRATONVM_JIT_VARHANDLE_BYTE_ARRAY_VIEW_READ=1 cratonvm ...
// HotSpot expectation: every line identical on every arm.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.nio.ByteOrder;

public class R21Fz4VhModes1 {
    static final int HOT = 100_000;

    static class F {
        byte b;
        boolean z;
        short sh;
        char c;
        byte b2;
        boolean z2;
        int i;
        long l;
        float f;
        double d;
        Object o;
        volatile int vi;
        volatile long vl;
    }

    // Never instantiated before the `late` pass.
    static final class G extends F {
        int extra = 5;
    }

    static class Lazy {
        static int s;

        static {
            System.out.println("clinit Lazy");
            s = 42;
        }
    }

    static final VarHandle B, Z, SH, C, B2, Z2, I, L, FL, D, O, VI, VL, LAZY;
    static final VarHandle IA = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle LA = MethodHandles.arrayElementVarHandle(long[].class);
    static final VarHandle BA = MethodHandles.arrayElementVarHandle(byte[].class);
    static final VarHandle OA = MethodHandles.arrayElementVarHandle(Object[].class);
    static final VarHandle VIL = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.LITTLE_ENDIAN);
    static final VarHandle VIB = MethodHandles.byteArrayViewVarHandle(int[].class, ByteOrder.BIG_ENDIAN);
    static final VarHandle VLL = MethodHandles.byteArrayViewVarHandle(long[].class, ByteOrder.LITTLE_ENDIAN);

    static {
        try {
            MethodHandles.Lookup lk = MethodHandles.lookup();
            B = lk.findVarHandle(F.class, "b", byte.class);
            Z = lk.findVarHandle(F.class, "z", boolean.class);
            SH = lk.findVarHandle(F.class, "sh", short.class);
            C = lk.findVarHandle(F.class, "c", char.class);
            B2 = lk.findVarHandle(F.class, "b2", byte.class);
            Z2 = lk.findVarHandle(F.class, "z2", boolean.class);
            I = lk.findVarHandle(F.class, "i", int.class);
            L = lk.findVarHandle(F.class, "l", long.class);
            FL = lk.findVarHandle(F.class, "f", float.class);
            D = lk.findVarHandle(F.class, "d", double.class);
            O = lk.findVarHandle(F.class, "o", Object.class);
            VI = lk.findVarHandle(F.class, "vi", int.class);
            VL = lk.findVarHandle(F.class, "vl", long.class);
            LAZY = lk.findStaticVarHandle(Lazy.class, "s", int.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    // Non-constant handle path.
    static VarHandle[] dyn = new VarHandle[2];

    static String fields(F x) {
        return "b=" + x.b + " z=" + x.z + " sh=" + x.sh + " c=" + (int) x.c + " b2=" + x.b2 + " z2=" + x.z2
            + " i=" + x.i + " l=" + x.l + " f=" + Float.floatToRawIntBits(x.f) + " d=" + Double.doubleToRawLongBits(x.d)
            + " o=" + x.o + " vi=" + x.vi + " vl=" + x.vl;
    }

    static String exc(Throwable e, boolean msg) {
        return e.getClass().getName() + (msg ? ":" + e.getMessage() : "");
    }

    interface Act {
        Object run() throws Throwable;
    }

    static String tryRow(Act a, boolean msg) {
        try {
            return "ok " + a.run();
        } catch (Throwable e) {
            return exc(e, msg);
        }
    }

    static void facts(String pass, F x, boolean msg) {
        x.b = 127;
        x.sh = 0x7fff;
        x.c = 0xffff;
        x.b2 = -1;
        x.z2 = true;
        x.z = false;
        StringBuilder sb = new StringBuilder();
        sb.append(" gaB=").append((byte) B.getAndAdd(x, (byte) 1));
        sb.append(" gaS=").append((short) SH.getAndAdd(x, (short) 1));
        sb.append(" gaC=").append((int) (char) C.getAndAdd(x, (char) 1));
        sb.append(" xorZ=").append((boolean) Z.getAndBitwiseXor(x, true));
        sb.append(" orB2=").append((byte) B2.getAndBitwiseAnd(x, (byte) 0x0f));
        sb.append(" casB=").append((boolean) B.compareAndSet(x, (byte) -128, (byte) 5));
        sb.append(" caeS=").append((short) SH.compareAndExchange(x, (short) 0, (short) 9));
        sb.append(" caeS2=").append((short) SH.compareAndExchange(x, (short) -32768, (short) 9));
        System.out.println(pass + " sub" + sb + " | " + fields(x));

        sb.setLength(0);
        x.f = Float.intBitsToFloat(0x7fc00000);
        sb.append(" casNaN=").append((boolean) FL.compareAndSet(x, Float.NaN, 1.0f));
        x.f = Float.intBitsToFloat(0x7fc00001);
        sb.append(" casNaNbits=").append((boolean) FL.compareAndSet(x, Float.NaN, 2.0f));
        x.f = -0.0f;
        sb.append(" casNegZero=").append((boolean) FL.compareAndSet(x, 0.0f, 3.0f));
        sb.append(" gaF=").append(Float.floatToRawIntBits((float) FL.getAndAdd(x, 0.5f)));
        x.d = -0.0;
        sb.append(" caeD=").append(Double.doubleToRawLongBits((double) D.compareAndExchange(x, 0.0, 4.0)));
        sb.append(" gaD=").append(Double.doubleToRawLongBits((double) D.getAndAdd(x, 1e300)));
        sb.append(" gaD2=").append(Double.doubleToRawLongBits((double) D.getAndAdd(x, Double.MAX_VALUE)));
        System.out.println(pass + " fp" + sb + " | " + fields(x));

        sb.setLength(0);
        x.i = Integer.MAX_VALUE;
        x.l = Long.MAX_VALUE;
        sb.append(" gaI=").append((int) I.getAndAdd(x, 1));
        sb.append(" gaL=").append((long) L.getAndAdd(x, 1L));
        sb.append(" caeI=").append((int) I.compareAndExchange(x, Integer.MIN_VALUE, 7));
        sb.append(" caeL=").append((long) L.compareAndExchangeAcquire(x, 0L, 7L));
        sb.append(" opq=").append((int) I.getOpaque(x));
        I.setRelease(x, 11);
        sb.append(" acq=").append((int) I.getAcquire(x));
        L.setOpaque(x, -2L);
        sb.append(" vol=").append((long) L.getVolatile(x));
        while (!I.weakCompareAndSetPlain(x, 11, 12)) {
            // spurious failure is legal; retry
        }
        sb.append(" weak=").append((int) I.get(x));
        sb.append(" weakFail=").append((boolean) I.weakCompareAndSet(x, 99, 13));
        sb.append(" gsO=").append(O.getAndSet(x, "new"));
        sb.append(" casO=").append((boolean) O.compareAndSet(x, "new", null));
        sb.append(" orVI=").append((int) VI.getAndBitwiseOr(x, 0x10));
        sb.append(" xorVL=").append((long) VL.getAndBitwiseXorRelease(x, -1L));
        sb.append(" gaVL=").append((long) VL.getAndAddAcquire(x, 3L));
        System.out.println(pass + " word" + sb + " | " + fields(x));

        System.out.println(pass + " unsup.gaZ " + tryRow(() -> (boolean) Z.getAndAdd(x, true), msg));
        System.out.println(pass + " unsup.gaO " + tryRow(() -> O.getAndAdd(x, "s"), msg));
        System.out.println(pass + " unsup.orF " + tryRow(() -> (float) FL.getAndBitwiseOr(x, 1.0f), msg));
        Object wrong = "not an F";
        System.out.println(pass + " recv.wrong " + tryRow(() -> (int) I.get(wrong), msg));
        F nul = null;
        System.out.println(pass + " recv.null " + tryRow(() -> (int) I.get(nul), false));
        System.out.println(pass + " recv.nullCas " + tryRow(() -> (boolean) I.compareAndSet(nul, 0, 1), false));

        int[] ia = {1, 2, 3, 4};
        long[] la = {Long.MAX_VALUE, 0, -1};
        byte[] ba = {127, -128, 0};
        System.out.println(pass + " arr.gaI " + (int) IA.getAndAdd(ia, 3, 10) + " " + ia[3]
            + " gaL " + (long) LA.getAndAdd(la, 0, 1L) + " " + la[0]
            + " gaB " + (byte) BA.getAndAdd(ba, 0, (byte) 1) + " " + ba[0] + " " + ba[1]
            + " caeI " + (int) IA.compareAndExchange(ia, 0, 1, 9) + " " + ia[0]);
        System.out.println(pass + " arr.m1 " + tryRow(() -> (int) IA.get(ia, -1), msg));
        System.out.println(pass + " arr.len " + tryRow(() -> (int) IA.getVolatile(ia, 4), msg));
        System.out.println(pass + " arr.lenL " + tryRow(() -> (long) LA.getAndAdd(la, 3, 1L), msg));
        int[] nia = null;
        System.out.println(pass + " arr.null " + tryRow(() -> (int) IA.get(nia, 0), false));
        Object[] strs = new String[2];
        System.out.println(pass + " arr.store " + tryRow(() -> {
            OA.set(strs, 0, Integer.valueOf(1));
            return strs[0];
        }, false));
        System.out.println(pass + " arr.storeOk " + tryRow(() -> {
            OA.setVolatile(strs, 1, "s");
            return strs[1];
        }, false));

        byte[] buf = new byte[16];
        for (int k = 0; k < buf.length; k++) {
            buf[k] = (byte) (k * 17 + 1);
        }
        sb.setLength(0);
        sb.append(" il0=").append(Integer.toHexString((int) VIL.get(buf, 0)));
        sb.append(" il1=").append(Integer.toHexString((int) VIL.get(buf, 1)));
        sb.append(" ib3=").append(Integer.toHexString((int) VIB.get(buf, 3)));
        sb.append(" ll5=").append(Long.toHexString((long) VLL.get(buf, 5)));
        VIB.set(buf, 7, 0x01020304);
        sb.append(" set7=").append(buf[7]).append(',').append(buf[10]);
        System.out.println(pass + " view" + sb);
        System.out.println(pass + " view.modes " + viewModes() + " trial=" + VIEW_GA + "," + VIEW_VOL_L + "," + VIEW_VOL_I);
        System.out.println(pass + " view.gaAligned " + tryRow(() -> Integer.toHexString((int) VIL.getAndAdd(buf, 4, 1)), msg));
        System.out.println(pass + " view.volAligned " + tryRow(() -> Integer.toHexString((int) VIL.getVolatile(buf, 8)), msg));
        System.out.println(pass + " view.casAligned " + tryRow(() -> (boolean) VIL.compareAndSet(buf, 0, 0, 5), msg));
        System.out.println(pass + " view.misVol " + tryRow(() -> (int) VIL.getVolatile(buf, 1), msg));
        System.out.println(pass + " view.misGa " + tryRow(() -> (int) VIL.getAndAdd(buf, 2, 1), msg));
        System.out.println(pass + " view.misL " + tryRow(() -> (long) VLL.getAcquire(buf, 4), msg));
        System.out.println(pass + " view.end " + tryRow(() -> (int) VIL.get(buf, 13), msg));
        System.out.println(pass + " view.endL " + tryRow(() -> (long) VLL.get(buf, 9), msg));
        System.out.println(pass + " view.neg " + tryRow(() -> (int) VIB.get(buf, -1), msg));
    }

    static long hotConst(F x, int n) {
        long h = 0;
        for (int k = 0; k < n; k++) {
            h = h * 31 + (byte) B.getAndAdd(x, (byte) 3);
            h = h * 31 + (short) SH.getAndAdd(x, (short) 1001);
            h = h * 31 + (char) C.getAndAdd(x, (char) 40000);
            h = h * 31 + ((boolean) Z.getAndBitwiseXor(x, true) ? 1 : 0);
            B2.set(x, (byte) k);
            Z2.setVolatile(x, (k & 1) == 0);
            h = h * 31 + (int) I.getAndAdd(x, k);
            h = h * 31 + (long) L.getAndAdd(x, 0x123456789L);
            if (!(boolean) I.compareAndSet(x, k, k + 1)) {
                h++;
            }
            h = h * 31 + (int) VI.getAndBitwiseXor(x, k);
            h = h * 31 + (long) VL.compareAndExchange(x, (long) k, (long) -k);
            h = h * 31 + Float.floatToRawIntBits((float) FL.getAndAdd(x, 0.25f));
            h = h * 31 + Double.doubleToRawLongBits((double) D.getAndAdd(x, 0.125));
            Object prev = O.getAndSet(x, (k & 1) == 0 ? "e" : "o");
            h = h * 31 + (prev == null ? 0 : prev.hashCode());
            h = h * 31 + (int) LAZY.getAndAdd(1);
        }
        return h;
    }

    static long hotDyn(F x, int n) {
        long h = 0;
        for (int k = 0; k < n; k++) {
            VarHandle a = dyn[k & 1];
            h = h * 31 + (int) a.getAndAdd(x, 1);
            h = h * 31 + (int) a.getVolatile(x);
        }
        return h;
    }

    // Byte-array views: which access modes the JDK supports is itself a fact
    // (HotSpot 25 throws UnsupportedOperationException for getAndAdd), so the
    // hot loops only use a mode the handle reports as supported.
    // Each flag also needs one trial call at index 0 to succeed, in case a mode
    // is reported as supported but refused at run time.
    static final boolean VIEW_GA = VIL.isAccessModeSupported(VarHandle.AccessMode.GET_AND_ADD) && trial(0);
    static final boolean VIEW_VOL_L = VLL.isAccessModeSupported(VarHandle.AccessMode.GET_VOLATILE) && trial(1);
    static final boolean VIEW_VOL_I = VIL.isAccessModeSupported(VarHandle.AccessMode.GET_VOLATILE) && trial(2);

    static boolean trial(int which) {
        byte[] scratch = new byte[16];
        try {
            if (which == 0) {
                int unused = (int) VIL.getAndAdd(scratch, 0, 1);
            } else if (which == 1) {
                long unused = (long) VLL.getVolatile(scratch, 0);
            } else {
                int unused = (int) VIL.getVolatile(scratch, 0);
            }
            return true;
        } catch (RuntimeException e) {
            return false;
        }
    }

    static String viewModes() {
        StringBuilder sb = new StringBuilder();
        VarHandle.AccessMode[] ms = {VarHandle.AccessMode.GET, VarHandle.AccessMode.SET,
            VarHandle.AccessMode.GET_VOLATILE, VarHandle.AccessMode.SET_RELEASE, VarHandle.AccessMode.GET_OPAQUE,
            VarHandle.AccessMode.COMPARE_AND_SET, VarHandle.AccessMode.COMPARE_AND_EXCHANGE,
            VarHandle.AccessMode.GET_AND_SET, VarHandle.AccessMode.GET_AND_ADD, VarHandle.AccessMode.GET_AND_BITWISE_OR};
        for (VarHandle.AccessMode m : ms) {
            sb.append(VIL.isAccessModeSupported(m) ? '1' : '0');
            sb.append(VLL.isAccessModeSupported(m) ? '1' : '0');
            sb.append(IA.isAccessModeSupported(m) ? '1' : '0');
            sb.append(' ');
        }
        return sb.toString().trim();
    }

    static long hotArr(int n) {
        int[] ia = new int[16];
        long[] la = new long[16];
        byte[] buf = new byte[64];
        long h = 0;
        for (int k = 0; k < n; k++) {
            int j = k & 15;
            h = h * 31 + (int) IA.getAndAdd(ia, j, k);
            h = h * 31 + (long) LA.getAndAdd(la, j, (long) k << 33);
            VIB.set(buf, j * 3, k);
            h = h * 31 + (int) VIL.get(buf, j * 3 + 1);
            if (VIEW_VOL_L) {
                h = h * 31 + (long) VLL.getVolatile(buf, (j & 7) * 8);
            }
            if (VIEW_GA) {
                h = h * 31 + (int) VIL.getAndAdd(buf, (j & 15) * 4, 1);
            } else {
                VIL.set(buf, (j & 15) * 4, (int) VIL.get(buf, (j & 15) * 4) + 1);
            }
        }
        return h;
    }

    static long misalignedHot(byte[] buf, int n) {
        if (!VIEW_VOL_I) {
            return -1;
        }
        int miss = 0;
        int unsup = 0;
        long h = 0;
        for (int k = 0; k < n; k++) {
            try {
                h = h * 31 + (int) VIL.getVolatile(buf, k & 7);
            } catch (IllegalStateException e) {
                miss++;
            } catch (UnsupportedOperationException e) {
                unsup++;
            }
        }
        return h * 1000 + miss + 100_000L * unsup;
    }

    static String values(F x) {
        return "fields " + fields(x) + " lazy=" + (int) LAZY.get();
    }

    public static void main(String[] args) throws Exception {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        dyn[0] = I;
        dyn[1] = lk.findVarHandle(F.class, "vi", int.class);
        System.out.println("start lazy=" + (int) LAZY.getVolatile());
        F x = new F();
        facts("cold", x, true);
        F y = new F();
        for (int p = 0; p < 2; p++) {
            System.out.println("hot" + p + " const=" + Long.toHexString(hotConst(y, HOT)) + " dyn="
                + Long.toHexString(hotDyn(y, HOT)) + " arr=" + Long.toHexString(hotArr(HOT))
                + " mis=" + misalignedHot(new byte[16], HOT / 10));
            System.out.println("hot" + p + " " + values(y));
        }
        facts("hot", new F(), false);
        F g = new G();
        System.out.println("late const=" + Long.toHexString(hotConst(g, HOT / 4)) + " dyn="
            + Long.toHexString(hotDyn(g, HOT / 4)));
        System.out.println("late " + values(g) + " extra=" + ((G) g).extra);
        facts("late", new G(), false);
        F z = new F();
        System.out.println("rehot const=" + Long.toHexString(hotConst(z, HOT)) + " dyn="
            + Long.toHexString(hotDyn(z, HOT)) + " arr=" + Long.toHexString(hotArr(HOT)));
        System.out.println("rehot " + values(z));
        facts("final", new F(), false);
        System.out.println("done");
    }
}
