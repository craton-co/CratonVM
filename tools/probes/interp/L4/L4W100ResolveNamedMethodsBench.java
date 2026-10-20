// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 100, lane L4: A/B bench for
// `CRATONVM_MHN_RESOLVE_NAMED_METHODS` (default ON since the i5 triage, A/B arm `=0`; page
// `docs/internal/fixed-bugs/interpreter-i89-L4-method-finder-per-class-and-per-handle-cost-CLOSED-20261018.md`,
// "Wave 100").
//
// `MethodHandleNatives.resolve` of a method member (every `Lookup.findStatic`
// / `findVirtual` / `findConstructor` whose member this thread has not
// resolved before) copied the WHOLE declared-method table of each class on
// its resolution path, about three times per class (`mhn_method_resolution`,
// the access check's "declares it" test, `declared_method_access_flags`),
// each entry four strings plus its decoded `Exceptions` / `Signature`. The
// switch asks the VM for the methods of the member's name only. The cost it
// removes grows with the class's method count, so the rows are:
//
//   resolve-fresh-wide    a fresh class with 200 static methods per
//                         iteration (defined untimed), `findStatic` of its
//                         last method timed
//   resolve-fresh-narrow  CONTROL: the same with 2 methods (should barely move)
//   resolve-jdk-cold      `findVirtual` / `findStatic` of 24 JDK methods on
//                         wide classes (String, StringBuilder, ArrayList,
//                         HashMap, Character, Math), each rep on a NEW thread
//                         so the per-thread resolve memos miss
//
// The fresh rows invoke each handle (untimed) and sum its answer; the JDK row
// counts the handles found.
// Timings: "TIME <row> med <ns> ns/op" (filter before diffing).
// Args: [n] (default 200) [reps] (default 5).
//
// Run: javac -d . L4W100ResolveNamedMethodsBench.java
//      java -cp . L4W100ResolveNamedMethodsBench                                (HotSpot)
//      cratonvm --jdk-only -cp . L4W100ResolveNamedMethodsBench
//      CRATONVM_MHN_RESOLVE_NAMED_METHODS=0 cratonvm --jdk-only -cp . L4W100ResolveNamedMethodsBench
//      (both modes, JIT and --nojit, interleaved; CRATONVM_DBG=mh-finder-census
//       shows the `mhn-resolve` phase mean directly)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived by arithmetic, n = 200, reps = 5)
// checksum resolve-fresh-wide 39800
// checksum resolve-fresh-narrow 200
// checksum resolve-jdk-cold 24
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;

public class L4W100ResolveNamedMethodsBench {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W100ResolveNamedMethodsBench.class.getClassLoader());
        }

        Class<?> define(byte[] bytes) {
            return defineClass(null, bytes, 0, bytes.length);
        }
    }

    static int seq;

    /** A public class `name` with `count` public static methods `m<i>()I` returning i. */
    static byte[] wide(String name, int count) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withSuperclass(ConstantDescs.CD_Object);
            for (int i = 0; i < count; i++) {
                final int k = i;
                cb.withMethodBody("m" + i, MethodTypeDesc.of(ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> code.loadConstant(k).ireturn());
            }
        });
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    /** Median over reps of (timed ns / n), and the first rep's checksum. */
    static void fresh(String row, int methods, int n, int reps) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType mt = MethodType.methodType(int.class);
        for (int r = 0; r < reps; r++) {
            Loader loader = new Loader();
            long timed = 0;
            long sum = 0;
            for (int i = 0; i < n; i++) {
                Class<?> c = loader.define(wide("W100Wide" + (seq++), methods));
                long t0 = System.nanoTime();
                MethodHandle mh = lookup.findStatic(c, "m" + (methods - 1), mt);
                timed += System.nanoTime() - t0;
                sum += (int) mh.invokeExact();
            }
            t[r] = timed / n;
            if (r == 0) {
                sum0 = sum;
            }
        }
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", row, median(t));
        System.out.println("checksum " + row + " " + sum0);
    }

    interface Find {
        MethodHandle find(MethodHandles.Lookup l) throws Throwable;
    }

    static final List<Find> JDK = List.of(
            l -> l.findVirtual(String.class, "length", MethodType.methodType(int.class)),
            l -> l.findVirtual(String.class, "isBlank", MethodType.methodType(boolean.class)),
            l -> l.findVirtual(String.class, "strip", MethodType.methodType(String.class)),
            l -> l.findVirtual(String.class, "indexOf", MethodType.methodType(int.class, String.class)),
            l -> l.findVirtual(String.class, "repeat", MethodType.methodType(String.class, int.class)),
            l -> l.findStatic(String.class, "valueOf", MethodType.methodType(String.class, int.class)),
            l -> l.findVirtual(StringBuilder.class, "reverse", MethodType.methodType(StringBuilder.class)),
            l -> l.findVirtual(StringBuilder.class, "length", MethodType.methodType(int.class)),
            l -> l.findVirtual(StringBuilder.class, "capacity", MethodType.methodType(int.class)),
            l -> l.findVirtual(StringBuilder.class, "append", MethodType.methodType(StringBuilder.class, int.class)),
            l -> l.findVirtual(ArrayList.class, "size", MethodType.methodType(int.class)),
            l -> l.findVirtual(ArrayList.class, "isEmpty", MethodType.methodType(boolean.class)),
            l -> l.findVirtual(ArrayList.class, "trimToSize", MethodType.methodType(void.class)),
            l -> l.findVirtual(ArrayList.class, "clear", MethodType.methodType(void.class)),
            l -> l.findVirtual(HashMap.class, "size", MethodType.methodType(int.class)),
            l -> l.findVirtual(HashMap.class, "isEmpty", MethodType.methodType(boolean.class)),
            l -> l.findVirtual(HashMap.class, "clear", MethodType.methodType(void.class)),
            l -> l.findVirtual(HashMap.class, "keySet", MethodType.methodType(java.util.Set.class)),
            l -> l.findStatic(Character.class, "isDigit", MethodType.methodType(boolean.class, char.class)),
            l -> l.findStatic(Character.class, "toUpperCase", MethodType.methodType(char.class, char.class)),
            l -> l.findStatic(Character.class, "isLetter", MethodType.methodType(boolean.class, int.class)),
            l -> l.findStatic(Math.class, "abs", MethodType.methodType(int.class, int.class)),
            l -> l.findStatic(Math.class, "max", MethodType.methodType(long.class, long.class, long.class)),
            l -> l.findStatic(Math.class, "floorMod", MethodType.methodType(int.class, int.class, int.class)));

    static void jdkCold(int reps) throws Throwable {
        long[] t = new long[reps];
        long[] count0 = new long[1];
        for (int r = 0; r < reps; r++) {
            long[] out = new long[2];
            Throwable[] failed = new Throwable[1];
            Thread th = new Thread(() -> {
                try {
                    MethodHandles.Lookup l = MethodHandles.publicLookup();
                    long timed = 0;
                    long count = 0;
                    for (Find f : JDK) {
                        long t0 = System.nanoTime();
                        MethodHandle mh = f.find(l);
                        timed += System.nanoTime() - t0;
                        if (mh != null) {
                            count++;
                        }
                    }
                    out[0] = timed / JDK.size();
                    out[1] = count;
                } catch (Throwable e) {
                    failed[0] = e;
                }
            });
            th.start();
            th.join();
            if (failed[0] != null) {
                throw failed[0];
            }
            t[r] = out[0];
            if (r == 0) {
                count0[0] = out[1];
            }
        }
        System.out.printf(Locale.ROOT, "TIME resolve-jdk-cold med %.1f ns/op%n", median(t));
        System.out.println("checksum resolve-jdk-cold " + count0[0]);
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        fresh("resolve-fresh-wide", 200, n, reps);
        fresh("resolve-fresh-narrow", 2, n, reps);
        jdkCold(reps);
    }
}
