// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i5 wave 102, lane L4: A/B bench for
// `CRATONVM_MHN_RESOLVE_NAMED_FIELDS` (default ON since the i5 triage, A/B arm `=0`; page
// `docs/internal/fixed-bugs/interpreter-i89-L4-method-finder-per-class-and-per-handle-cost-CLOSED-20261018.md`,
// "Wave 102"), the field half of `CRATONVM_MHN_RESOLVE_NAMED_METHODS`.
//
// `MethodHandleNatives.resolve` of a field member (every `Lookup.findGetter`
// / `findStaticGetter` / `findVarHandle` this thread has not resolved before)
// copied the WHOLE declared-field table of each class on its resolution path
// (`mhn_field_search`, then `declared_field_access_flags`, then the declaring
// class reveal), two strings per entry. The switch asks the VM for the fields
// of the member's name only. Rows:
//
//   field-fresh-wide     a fresh class with 200 static final int fields per
//                        iteration (defined untimed), `findStaticGetter` of
//                        its last field timed
//   field-fresh-inherit  a fresh subclass (no fields) of a fresh 200-field
//                        class, `findGetter` of the superclass's last
//                        instance field through the subclass
//   field-fresh-narrow   CONTROL: 2 static fields (should barely move)
//   field-jdk-cold       16 JDK field finders on field-rich classes
//                        (Character, Integer, Long, Double, Float, Math,
//                        Short, Byte), each rep on a NEW thread
//
// The fresh rows read each handle (untimed) and sum the answer; the JDK row
// counts the handles found.
// Timings: "TIME <row> med <ns> ns/op" (filter before diffing).
// Args: [n] (default 200) [reps] (default 5).
//
// Run: javac -d . L4W102ResolveNamedFieldsBench.java
//      java -cp . L4W102ResolveNamedFieldsBench                                (HotSpot)
//      cratonvm --jdk-only -cp . L4W102ResolveNamedFieldsBench
//      CRATONVM_MHN_RESOLVE_NAMED_FIELDS=0 cratonvm --jdk-only -cp . L4W102ResolveNamedFieldsBench
//      (both modes, JIT and --nojit, interleaved)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived by arithmetic, n = 200, reps = 5)
// checksum field-fresh-wide 39800
// checksum field-fresh-inherit 0
// checksum field-fresh-narrow 200
// checksum field-jdk-cold 16
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.ConstantValueAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.util.Arrays;
import java.util.List;
import java.util.Locale;

public class L4W102ResolveNamedFieldsBench {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W102ResolveNamedFieldsBench.class.getClassLoader());
        }

        Class<?> define(byte[] bytes) {
            return defineClass(null, bytes, 0, bytes.length);
        }
    }

    static int seq;

    /** A public class `name` with `count` public static final int fields `f<i>` = i. */
    static byte[] wideStatic(String name, int count) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withSuperclass(ConstantDescs.CD_Object);
            for (int i = 0; i < count; i++) {
                final int k = i;
                cb.withField("f" + i, ConstantDescs.CD_int, fb -> fb
                        .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC | ClassFile.ACC_FINAL)
                        .with(ConstantValueAttribute.of(k)));
            }
        });
    }

    /** A public class `name` with `count` public int instance fields and a public no-arg constructor. */
    static byte[] wideInstance(String name, int count) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withSuperclass(ConstantDescs.CD_Object);
            for (int i = 0; i < count; i++) {
                cb.withField("g" + i, ConstantDescs.CD_int, ClassFile.ACC_PUBLIC);
            }
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, "<init>",
                                    MethodTypeDesc.of(ConstantDescs.CD_void))
                            .return_());
        });
    }

    /** A public empty subclass `name` of `sup`. */
    static byte[] sub(String name, String sup) {
        ClassDesc s = ClassDesc.of(sup);
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC);
            cb.withSuperclass(s);
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(s, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                            .return_());
        });
    }

    static double median(long[] xs) {
        long[] c = xs.clone();
        Arrays.sort(c);
        int m = c.length / 2;
        return c.length % 2 == 1 ? c[m] : (c[m - 1] + c[m]) / 2.0;
    }

    static void freshStatic(String row, int fields, int n, int reps) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int r = 0; r < reps; r++) {
            Loader loader = new Loader();
            long timed = 0;
            long sum = 0;
            for (int i = 0; i < n; i++) {
                Class<?> c = loader.define(wideStatic("W102Wide" + (seq++), fields));
                long t0 = System.nanoTime();
                MethodHandle mh = lookup.findStaticGetter(c, "f" + (fields - 1), int.class);
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

    static void freshInherit(int n, int reps) throws Throwable {
        long[] t = new long[reps];
        long sum0 = 0;
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        for (int r = 0; r < reps; r++) {
            Loader loader = new Loader();
            long timed = 0;
            long sum = 0;
            for (int i = 0; i < n; i++) {
                String sup = "W102Base" + (seq++);
                loader.define(wideInstance(sup, 200));
                Class<?> c = loader.define(sub("W102Sub" + (seq++), sup));
                Object o = c.getConstructor().newInstance();
                long t0 = System.nanoTime();
                MethodHandle mh = lookup.findGetter(c, "g199", int.class);
                timed += System.nanoTime() - t0;
                sum += (int) mh.invoke(o);
            }
            t[r] = timed / n;
            if (r == 0) {
                sum0 = sum;
            }
        }
        System.out.printf(Locale.ROOT, "TIME field-fresh-inherit med %.1f ns/op%n", median(t));
        System.out.println("checksum field-fresh-inherit " + sum0);
    }

    interface Find {
        MethodHandle find(MethodHandles.Lookup l) throws Throwable;
    }

    static final List<Find> JDK = List.of(
            l -> l.findStaticGetter(Character.class, "MAX_VALUE", char.class),
            l -> l.findStaticGetter(Character.class, "MIN_RADIX", int.class),
            l -> l.findStaticGetter(Character.class, "UNASSIGNED", byte.class),
            l -> l.findStaticGetter(Character.class, "MAX_CODE_POINT", int.class),
            l -> l.findStaticGetter(Integer.class, "MAX_VALUE", int.class),
            l -> l.findStaticGetter(Integer.class, "SIZE", int.class),
            l -> l.findStaticGetter(Long.class, "MIN_VALUE", long.class),
            l -> l.findStaticGetter(Long.class, "BYTES", int.class),
            l -> l.findStaticGetter(Double.class, "NaN", double.class),
            l -> l.findStaticGetter(Double.class, "MAX_EXPONENT", int.class),
            l -> l.findStaticGetter(Float.class, "MIN_NORMAL", float.class),
            l -> l.findStaticGetter(Math.class, "PI", double.class),
            l -> l.findStaticGetter(Math.class, "TAU", double.class),
            l -> l.findStaticGetter(Short.class, "MAX_VALUE", short.class),
            l -> l.findStaticGetter(Byte.class, "MIN_VALUE", byte.class),
            l -> l.findStaticGetter(Character.class, "SURROGATE", byte.class));

    static void jdkCold(int reps) throws Throwable {
        long[] t = new long[reps];
        long count0 = 0;
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
                count0 = out[1];
            }
        }
        System.out.printf(Locale.ROOT, "TIME field-jdk-cold med %.1f ns/op%n", median(t));
        System.out.println("checksum field-jdk-cold " + count0);
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        freshStatic("field-fresh-wide", 200, n, reps);
        freshInherit(n, reps);
        freshStatic("field-fresh-narrow", 2, n, reps);
        jdkCold(reps);
    }
}
