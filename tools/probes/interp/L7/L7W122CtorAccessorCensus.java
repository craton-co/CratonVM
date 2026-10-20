// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// probe + bench (census driver)
//
// Interpreter round i6 wave 122, lane L7: the constructor census i88-L3 asks for
// (docs/internal/fixed-bugs/interpreter-i88-L3-proposal-first-use-reflective-constructor-cost-CLOSED-20261224.md,
// "Next step, unchanged from wave 94": the `CRATONVM_DBG_INTERP_FRAMES` census of
// `newConstructorAccessor` and of the cached path, per fresh class). Every row works on
// classes defined fresh (one per iteration, a no-argument public constructor, built with
// java.lang.classfile) so each pays the first-use cost, and each phase is its own row, so a run
// restricted to one row (`only <row>`) gives that phase's interpreted-frame census alone:
//
//   getConstructor        the first `getConstructor()` of a fresh class (ReflectionData,
//                         the declared-constructor listing, `Constructor.copy()`)
//   newInstance-1         the first `newInstance()` of that constructor:
//                         `acquireConstructorAccessor` -> `newConstructorAccessor` (the
//                         `JLIA.unreflectConstructor` handle, `makeConstructorHandle`'s `asType`,
//                         `DirectConstructorHandleAccessor`) and the first call through it
//   newInstance-2         the second call: the cached accessor and its handle chain only
//   newInstance-hot       the 20th call on the same constructor (the steady cached path)
//   unreflect             `MethodHandles.lookup().unreflectConstructor(k)` on a fresh class:
//                         the handle half of `newConstructorAccessor` without the accessor
//   unreflect-invoke-1    the first `invoke()` of that handle (its linkage and form chain)
//
// Reading: newInstance-1 minus (unreflect + unreflect-invoke-1) bounds the accessor object's own
// share; newInstance-2 minus newInstance-hot is what is still first-use after the accessor exists.
// HotSpot 25.0.3 on the lane's machine, med ns (default flags / `-Xint`): getConstructor
// 9000 / 7000, newInstance-1 53250 / 50600, newInstance-2 1800 / 3800, newInstance-hot 1000 /
// 1800, unreflect 14900 / 37550, unreflect-invoke-1 8550 / 9800.
//
// Run:
//   java -cp <dir> L7W122CtorAccessorCensus                          (HotSpot; also -Xint)
//   cratonvm --jdk-only -cp <dir> L7W122CtorAccessorCensus
//   cratonvm --jdk-only --nojit -cp <dir> L7W122CtorAccessorCensus
// The census, one phase per run (the census prints at exit; compare the frame counts per row):
//   for r in getConstructor newInstance-1 newInstance-2 newInstance-hot unreflect unreflect-invoke-1; do
//     CRATONVM_DBG_INTERP_FRAMES=1 cratonvm --jdk-only -cp <dir> L7W122CtorAccessorCensus 200 only $r 2> frames-$r.txt
//   done
//   (and with CRATONVM_DBG_GDC_PROF=1 for the native half, as the page's wave-89 census)
// Output: `checksum <row> <n>` lines (compared) and `TIME <row> min <ns> med <ns> ns/op`.
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// checksum getConstructor 200
// checksum newInstance-1 200
// checksum newInstance-2 200
// checksum newInstance-hot 200
// checksum unreflect 200
// checksum unreflect-invoke-1 200
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.reflect.Constructor;
import java.util.Arrays;
import java.util.Locale;

public class L7W122CtorAccessorCensus {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L7W122CtorAccessorCensus.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;
    static Object sink;

    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
        });
    }

    /** A fresh, defined and initialised class of `loader`. */
    static Class<?> fresh(Loader loader) throws Throwable {
        String name = "W122Ctor" + (classes++);
        Class<?> c = loader.define(name, build(name));
        Class.forName(name, true, loader);
        return c;
    }

    static void print(boolean print, String name, long[] t, int ok) {
        if (!print) {
            return;
        }
        long[] c = t.clone();
        Arrays.sort(c);
        double med = c.length % 2 == 1 ? c[c.length / 2] : (c[c.length / 2 - 1] + c[c.length / 2]) / 2.0;
        System.out.println("checksum " + name + " " + ok);
        System.out.printf(Locale.ROOT, "TIME %s min %d med %.1f ns/op%n", name, c[0], med);
    }

    static boolean wanted(String only, String row) {
        return only == null || only.equals(row);
    }

    static void pass(boolean print, int count, String only) throws Throwable {
        Loader loader = new Loader();
        MethodHandles.Lookup lookup = MethodHandles.lookup();

        long[] getCtor = new long[count];
        long[] first = new long[count];
        long[] second = new long[count];
        long[] hot = new long[count];
        int okGet = 0;
        int okFirst = 0;
        int okSecond = 0;
        int okHot = 0;
        boolean ctorRows = wanted(only, "getConstructor") || wanted(only, "newInstance-1")
                || wanted(only, "newInstance-2") || wanted(only, "newInstance-hot");
        for (int i = 0; ctorRows && i < count; i++) {
            Class<?> c = fresh(loader);
            long t0 = System.nanoTime();
            Constructor<?> k = c.getConstructor();
            getCtor[i] = System.nanoTime() - t0;
            okGet += k.getParameterCount() == 0 ? 1 : 0;
            if (!(wanted(only, "newInstance-1") || wanted(only, "newInstance-2")
                    || wanted(only, "newInstance-hot"))) {
                continue;
            }
            long t1 = System.nanoTime();
            Object o1 = k.newInstance();
            first[i] = System.nanoTime() - t1;
            okFirst += o1.getClass() == c ? 1 : 0;
            long t2 = System.nanoTime();
            Object o2 = k.newInstance();
            second[i] = System.nanoTime() - t2;
            okSecond += o2.getClass() == c && o2 != o1 ? 1 : 0;
            for (int j = 0; j < 17; j++) {
                sink = k.newInstance();
            }
            long t3 = System.nanoTime();
            Object o20 = k.newInstance();
            hot[i] = System.nanoTime() - t3;
            okHot += o20.getClass() == c ? 1 : 0;
        }

        long[] unreflect = new long[count];
        long[] invoke1 = new long[count];
        int okUnreflect = 0;
        int okInvoke1 = 0;
        boolean mhRows = wanted(only, "unreflect") || wanted(only, "unreflect-invoke-1");
        for (int i = 0; mhRows && i < count; i++) {
            Class<?> c = fresh(loader);
            Constructor<?> k = c.getConstructor();
            long t0 = System.nanoTime();
            MethodHandle h = lookup.unreflectConstructor(k);
            unreflect[i] = System.nanoTime() - t0;
            okUnreflect += h.type().parameterCount() == 0 ? 1 : 0;
            long t1 = System.nanoTime();
            Object o = h.invoke();
            invoke1[i] = System.nanoTime() - t1;
            okInvoke1 += o.getClass() == c ? 1 : 0;
        }

        if (ctorRows) {
            print(print && wanted(only, "getConstructor"), "getConstructor", getCtor, okGet);
            print(print && wanted(only, "newInstance-1"), "newInstance-1", first, okFirst);
            print(print && wanted(only, "newInstance-2"), "newInstance-2", second, okSecond);
            print(print && wanted(only, "newInstance-hot"), "newInstance-hot", hot, okHot);
        }
        if (mhRows) {
            print(print && wanted(only, "unreflect"), "unreflect", unreflect, okUnreflect);
            print(print && wanted(only, "unreflect-invoke-1"), "unreflect-invoke-1", invoke1, okInvoke1);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        String only = args.length > 2 && args[1].equals("only") ? args[2] : null;
        // A short unprinted pass, so the JDK's own first-use of reflection and of the
        // classfile API is not in the first fresh class's rows.
        pass(false, 20, only);
        pass(true, n, only);
    }
}
