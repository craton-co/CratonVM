// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// bench
//
// Interpreter round i4 wave 91, lane L3 (`CRATONVM_FIELD_NAME_RING_DURABLE`,
// i88-L3-proposal-first-use-reflective-constructor-cost): does a class
// definition between two reflective mints make the second one slower?
//
// Every define moves CratonVM's class-provenance epoch, and the native by-name
// field accessors' per-thread rings (`get_field_by_name` / `set_field_by_name`)
// honoured an entry only under the epoch it was filled in. So a mint right
// after a define (a fresh class's getConstructor, getDeclaredField,
// getDeclaredMethod) re-walked the mirror class's hierarchy under the
// class-manager lock for every field it wrote. Each op is timed twice, on
// fresh, initialized classes:
//
//   <op>-interleaved   define + initialize a class, then time the op on it
//                      (a define just before every timed op)
//   <op>-batched       define + initialize all the classes first, then time
//                      the op on each (no define between timed ops)
//
// interleaved minus batched is what the define costs the next mint. With
// CRATONVM_FIELD_NAME_RING_DURABLE=1 it should shrink toward zero; the
// batched rows are the control (they must not move).
//
// Output: "row <name> min <ns> med <ns> ns/op" and "checksum <name> <n>".
// Run: javac -d . L3W91FieldNameRingBench.java
//      java -cp . L3W91FieldNameRingBench                                        (HotSpot)
//      cratonvm --jdk-only -cp . L3W91FieldNameRingBench
//      CRATONVM_FIELD_NAME_RING_DURABLE=1 cratonvm --jdk-only -cp . L3W91FieldNameRingBench
//      (and the same pair with --compatible; an optional argument sets the
//      printed count, default 200)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// checksum ctor-interleaved 200
// checksum ctor-batched 200
// checksum field-interleaved 200
// checksum field-batched 200
// checksum method-interleaved 200
// checksum method-batched 200
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.util.Arrays;
import java.util.Locale;

public class L3W91FieldNameRingBench {
    static final MethodTypeDesc VOID = MethodTypeDesc.of(ConstantDescs.CD_void);

    static final class Loader extends ClassLoader {
        Loader() {
            super(L3W91FieldNameRingBench.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static int classes;
    static Object sink;

    /** A public class with a public no-arg constructor, an int field f and a void method m. */
    static byte[] build(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("f", ConstantDescs.CD_int, ClassFile.ACC_PUBLIC);
            cb.withMethodBody(ConstantDescs.INIT_NAME, VOID, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, VOID)
                            .return_());
            cb.withMethodBody("m", VOID, ClassFile.ACC_PUBLIC, code -> code.return_());
        });
    }

    static Class<?> fresh(Loader loader) throws Throwable {
        String name = "Rr" + (classes++);
        Class<?> c = loader.define(name, build(name));
        Class.forName(name, true, loader);
        return c;
    }

    /** One reflective mint on a fresh class; 1 when its answer is the expected one. */
    static int op(String kind, Class<?> c) throws Throwable {
        switch (kind) {
            case "ctor": {
                Constructor<?> k = c.getConstructor();
                sink = k;
                return k.getParameterCount() == 0 ? 1 : 0;
            }
            case "field": {
                Field f = c.getDeclaredField("f");
                sink = f;
                return f.getType() == int.class ? 1 : 0;
            }
            default: {
                Method m = c.getDeclaredMethod("m");
                sink = m;
                return m.getReturnType() == void.class ? 1 : 0;
            }
        }
    }

    static void print(String name, long[] t, int ok) {
        long[] c = t.clone();
        Arrays.sort(c);
        double med = c.length % 2 == 1 ? c[c.length / 2] : (c[c.length / 2 - 1] + c[c.length / 2]) / 2.0;
        System.out.printf(Locale.ROOT, "row %s min %d med %.1f ns/op%n", name, c[0], med);
        System.out.println("checksum " + name + " " + ok);
    }

    static void interleaved(String kind, int count, boolean print) throws Throwable {
        Loader loader = new Loader();
        long[] t = new long[count];
        int ok = 0;
        for (int i = 0; i < count; i++) {
            Class<?> c = fresh(loader);
            long t0 = System.nanoTime();
            ok += op(kind, c);
            t[i] = System.nanoTime() - t0;
        }
        if (print) {
            print(kind + "-interleaved", t, ok);
        }
    }

    static void batched(String kind, int count, boolean print) throws Throwable {
        Loader loader = new Loader();
        Class<?>[] cs = new Class<?>[count];
        for (int i = 0; i < count; i++) {
            cs[i] = fresh(loader);
        }
        long[] t = new long[count];
        int ok = 0;
        for (int i = 0; i < count; i++) {
            long t0 = System.nanoTime();
            ok += op(kind, cs[i]);
            t[i] = System.nanoTime() - t0;
        }
        if (print) {
            print(kind + "-batched", t, ok);
        }
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        String[] kinds = {"ctor", "field", "method"};
        // Warm-up: past the JIT invocation threshold on every Java frame of
        // the three ops (getConstructor / getDeclaredField / getDeclaredMethod
        // and their copy chains), with both shapes.
        for (int round = 0; round < 2; round++) {
            for (String kind : kinds) {
                interleaved(kind, 300, false);
                batched(kind, 300, false);
            }
        }
        for (String kind : kinds) {
            interleaved(kind, n, true);
            batched(kind, n, true);
        }
    }
}
