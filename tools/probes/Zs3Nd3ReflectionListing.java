// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.Serializable;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * zs s3/nd3: the declared reflection listings answer TYPED arrays
 * ({@code Field[]}, {@code Method[]}, {@code Constructor[]}), including their
 * empty arms (a primitive, an array type, an interface without constructors),
 * from the first call of the program on (ZGC round zs, wave s3, lane nd3;
 * {@code zgc-zs2nd2-native-door-residuals-SUPERSEDED-20261019.md} section 6).
 *
 * <p>The VM defect it guards: the listing natives looked their element class
 * up by name and, when it was not loaded yet, allocated the result over class
 * id 0, so the first listing of such a VM was not a {@code Field[]}. A real
 * JDK boot normally loads {@code Field} early, so on a host binary this row is
 * a regression guard for the typed answer (the unit tests
 * {@code zs3nd3_tests::listing_tests} reach the not-yet-loaded state). Every
 * line is deterministic.
 */
public class Zs3Nd3ReflectionListing {
    interface NoCtors extends Serializable {
        int value();
    }

    static final class Holder {
        public int a;
        private String b;

        Holder() {}

        Holder(int a) {
            this.a = a;
        }

        int twice() {
            return a * 2;
        }
    }

    static void row(String what, Object array, int length) {
        System.out.println(what + " " + array.getClass().getName() + " len=" + length);
    }

    public static void main(String[] args) {
        // First listings of the program, before anything else reflects.
        Field[] f = Holder.class.getDeclaredFields();
        row("Holder.fields", f, f.length);
        Method[] m = Holder.class.getDeclaredMethods();
        row("Holder.methods", m, m.length);
        Constructor<?>[] c = Holder.class.getDeclaredConstructors();
        row("Holder.ctors", c, c.length);
        // The empty arms.
        Class<?>[] kinds = {int.class, int[].class, String[].class, NoCtors.class, void.class};
        for (Class<?> k : kinds) {
            Field[] kf = k.getDeclaredFields();
            row(k.getName() + ".fields", kf, kf.length);
            Method[] km = k.getDeclaredMethods();
            row(k.getName() + ".methods", km, km.length);
            Constructor<?>[] kc = k.getDeclaredConstructors();
            row(k.getName() + ".ctors", kc, kc.length);
        }
        // The public listings (they share the declared natives on the JDK path).
        Field[] pf = Holder.class.getFields();
        row("Holder.publicFields", pf, pf.length);
        Constructor<?>[] pc = NoCtors.class.getConstructors();
        row("NoCtors.publicCtors", pc, pc.length);
        // Typed: assignable without a cast failure.
        Object any = Holder.class.getDeclaredFields();
        System.out.println("isFieldArray=" + (any instanceof Field[]));
    }
}
