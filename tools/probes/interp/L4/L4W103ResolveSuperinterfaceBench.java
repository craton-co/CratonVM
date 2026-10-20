// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 103, lane L4: A/B bench for
// `CRATONVM_MHN_RESOLVE_NAMED_SUPERINTERFACES` (default ON since the w104
// triage: the A/B arm is now `=0`; page
// `docs/internal/fixed-bugs/interpreter-i89-L4-method-finder-per-class-and-per-handle-cost-CLOSED-20261018.md`,
// "Wave 103"). Wave 105 adds `iface-fresh-many` for the wave-104 parity fix
// (page `i104-L4-superinterface-resolution-takes-the-first-lifo-find`): the
// superinterface walk no longer stops at its first find, so a class with many
// superinterfaces pays for every one. That fix has no switch; compare this
// row between a pre-w104 binary and the current one (and both arms of the
// named switch, which decides what each visited interface costs).
//
// `MethodHandleNatives.resolve` of an instance method that the class's
// superclass chain does not declare (a default method the class inherits from
// an interface) asks `lk_superinterface_declares_instance_method`, which
// copied the WHOLE declared-method table of every superinterface it visited
// (`declared_methods`: four strings and the decoded `Exceptions` /
// `Signature` per method) to find one name. The switch asks the VM for the
// methods of that name only (`declared_methods_named`), as
// `CRATONVM_MHN_RESOLVE_NAMED_METHODS` already does for the class chain.
//
//   iface-fresh-wide    a fresh interface with 200 default methods `m<i>()I`
//                       and a fresh empty class implementing it per iteration
//                       (defined untimed); `findVirtual(class, "m199")` timed
//   iface-fresh-narrow  CONTROL: the same with 2 default methods
//   iface-fresh-many    a fresh class implementing 16 fresh EMPTY interfaces
//                       and, LAST in its `implements` list (the walk's first
//                       pop), one with 2 defaults; `findVirtual(class, "m1")`
//                       timed. Before wave 104 the walk stopped at that first
//                       find; now it visits all 17 (wave 105)
//   iface-jdk-cold      `findVirtual` of 8 inherited JDK default methods
//                       (`Collection.stream` on ArrayList, ...), on 16 NEW
//                       threads per rep so the per-thread resolve memos miss
//
// The fresh rows invoke each handle (untimed) and sum its answer; the JDK row
// counts the handles found.
// Timings: "TIME <row> med <ns> ns/op" (filter before diffing).
// Args: [n] (default 200) [reps] (default 5).
//
// Run: javac -d . L4W103ResolveSuperinterfaceBench.java
//      java -cp . L4W103ResolveSuperinterfaceBench                                  (HotSpot)
//      cratonvm --jdk-only -cp . L4W103ResolveSuperinterfaceBench
//      CRATONVM_MHN_RESOLVE_NAMED_SUPERINTERFACES=0 cratonvm --jdk-only -cp . L4W103ResolveSuperinterfaceBench
//      (both modes, JIT and --nojit, interleaved; CRATONVM_DBG=mh-finder-census
//       shows the `mhn-resolve` phase mean directly)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived by arithmetic, n = 200, reps = 5)
// checksum iface-fresh-wide 39800
// checksum iface-fresh-narrow 200
// checksum iface-fresh-many 200
// checksum iface-jdk-cold 8
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
import java.util.HashSet;
import java.util.LinkedList;
import java.util.List;
import java.util.Locale;
import java.util.TreeMap;
import java.util.function.Predicate;

public class L4W103ResolveSuperinterfaceBench {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W103ResolveSuperinterfaceBench.class.getClassLoader());
        }

        Class<?> define(byte[] bytes) {
            return defineClass(null, bytes, 0, bytes.length);
        }
    }

    static int seq;

    /** A public interface `name` with `count` public default methods `m<i>()I` returning i. */
    static byte[] iface(String name, int count) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT);
            cb.withSuperclass(ConstantDescs.CD_Object);
            for (int i = 0; i < count; i++) {
                final int k = i;
                cb.withMethodBody("m" + i, MethodTypeDesc.of(ConstantDescs.CD_int),
                        ClassFile.ACC_PUBLIC,
                        code -> code.loadConstant(k).ireturn());
            }
        });
    }

    /** A public class `name` implementing `iface`, declaring only its constructor. */
    static byte[] impl(String name, String iface) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withInterfaceSymbols(ClassDesc.of(iface));
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
        });
    }

    /** A public class `name` implementing `ifaces` (in that order), declaring only its constructor. */
    static byte[] implAll(String name, ClassDesc[] ifaces) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ConstantDescs.CD_Object);
            cb.withInterfaceSymbols(ifaces);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
        });
    }

    /** Wave 105: `iface-fresh-many` (see the header); median ns per find, first rep's checksum. */
    static void freshMany(String row, int empties, int n, int reps) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType mt = MethodType.methodType(int.class);
        for (int r = 0; r < reps; r++) {
            Loader loader = new Loader();
            long timed = 0;
            long sum = 0;
            for (int i = 0; i < n; i++) {
                int k = seq++;
                ClassDesc[] ifaces = new ClassDesc[empties + 1];
                for (int e = 0; e < empties; e++) {
                    String en = "W105Empty" + k + "_" + e;
                    loader.define(iface(en, 0));
                    ifaces[e] = ClassDesc.of(en);
                }
                String in = "W105Iface" + k;
                loader.define(iface(in, 2));
                ifaces[empties] = ClassDesc.of(in);
                Class<?> c = loader.define(implAll("W105Impl" + k, ifaces));
                Object o = c.getConstructor().newInstance();
                long t0 = System.nanoTime();
                MethodHandle mh = lookup.findVirtual(c, "m1", mt);
                timed += System.nanoTime() - t0;
                sum += (int) mh.invoke(o);
            }
            t[r] = timed / n;
            if (r == 0) {
                sum0 = sum;
            }
        }
        System.out.printf(Locale.ROOT, "TIME %s med %.1f ns/op%n", row, median(t));
        System.out.println("checksum " + row + " " + sum0);
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
                int k = seq++;
                String in = "W103Iface" + k;
                loader.define(iface(in, methods));
                Class<?> c = loader.define(impl("W103Impl" + k, in));
                Object o = c.getConstructor().newInstance();
                long t0 = System.nanoTime();
                MethodHandle mh = lookup.findVirtual(c, "m" + (methods - 1), mt);
                timed += System.nanoTime() - t0;
                sum += (int) mh.invoke(o);
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
            l -> l.findVirtual(ArrayList.class, "stream", MethodType.methodType(java.util.stream.Stream.class)),
            l -> l.findVirtual(ArrayList.class, "parallelStream",
                    MethodType.methodType(java.util.stream.Stream.class)),
            l -> l.findVirtual(LinkedList.class, "stream", MethodType.methodType(java.util.stream.Stream.class)),
            l -> l.findVirtual(HashSet.class, "stream", MethodType.methodType(java.util.stream.Stream.class)),
            l -> l.findVirtual(HashSet.class, "toArray",
                    MethodType.methodType(Object[].class, java.util.function.IntFunction.class)),
            l -> l.findVirtual(TreeMap.class, "getOrDefault",
                    MethodType.methodType(Object.class, Object.class, Object.class)),
            l -> l.findVirtual(LinkedList.class, "removeIf", MethodType.methodType(boolean.class, Predicate.class)),
            l -> l.findVirtual(ArrayList.class, "toArray",
                    MethodType.methodType(Object[].class, java.util.function.IntFunction.class)));

    static void jdkCold(int reps) throws Throwable {
        long[] t = new long[reps];
        long[] count0 = new long[1];
        // Wave 104: THREADS fresh threads per rep (8 finds each was too few
        // samples; the w103 --nojit row swung 1.45x with the switch doing less).
        final int threads = 16;
        for (int r = 0; r < reps; r++) {
            long repTimed = 0;
            for (int k = 0; k < threads; k++) {
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
                        out[0] = timed;
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
                repTimed += out[0];
                if (r == 0 && k == 0) {
                    count0[0] = out[1];
                }
            }
            t[r] = repTimed / ((long) threads * JDK.size());
        }
        System.out.printf(Locale.ROOT, "TIME iface-jdk-cold med %.1f ns/op%n", median(t));
        System.out.println("checksum iface-jdk-cold " + count0[0]);
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        fresh("iface-fresh-wide", 200, n, reps);
        fresh("iface-fresh-narrow", 2, n, reps);
        freshMany("iface-fresh-many", 16, n, reps);
        jdkCold(reps);
    }
}
