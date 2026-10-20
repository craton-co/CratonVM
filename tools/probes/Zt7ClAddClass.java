// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.nio.ByteBuffer;
import java.util.ArrayList;
import java.util.List;

/**
 * zt t7/cl: does every define door record the class in its loader's
 * {@code ClassLoader.classes} list, as HotSpot's
 * {@code SystemDictionary::define_instance_class} does through the Java upcall
 * {@code ClassLoader.addClass} (ZGC round zt, wave t7, lane cl; t6/nd4
 * finding 14)?
 *
 * <p>The list is read through a {@code privateLookupIn} getter (core
 * reflection filters every {@code ClassLoader} field), so the run needs
 * {@code --add-opens java.base/java.lang=ALL-UNNAMED}. One fresh loader
 * defines {@code n} classes through {@code defineClass(String, byte[], int,
 * int)} ({@code defineClass1}), one through a direct {@code ByteBuffer}
 * ({@code defineClass2}), one duplicate (refused before {@code addClass}), one
 * through {@code Lookup.defineClass} ({@code defineClass0}) and one hidden class
 * (never added). A second loader that defined nothing stays empty.
 *
 * <p>stdout (deterministic): one line per step with the list's size and
 * whether it holds exactly the defined classes in definition order
 * ({@code same=}). HotSpot 25 is the reference; a VM that never calls
 * {@code addClass} prints {@code size=0 same=false} on every step. Kill
 * control: {@code CRATONVM_DEFINE_ADD_CLASS=0} (and the base commit) print
 * exactly that.
 *
 * <p>Usage: {@code java --add-opens java.base/java.lang=ALL-UNNAMED
 * Zt7ClAddClass [n] [app]} (default 12: crosses the list's first growth at
 * 10; {@code app} also prints the application loader's list, a gap row).
 */
public class Zt7ClAddClass {
    static final class Ldr extends ClassLoader {
        Ldr() {
            super(null);
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }

        Class<?> defDirect(String name, byte[] b) {
            ByteBuffer bb = ByteBuffer.allocateDirect(b.length);
            bb.put(b);
            bb.flip();
            return defineClass(name, bb, null);
        }
    }

    /** {@code public class <name> extends java.lang.Object {}} (version 52). */
    static byte[] classBytes(String internalName) throws IOException {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bos);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(52);
        out.writeShort(5); // cp 1..4
        out.writeByte(7);
        out.writeShort(2); // #1 Class #2
        out.writeByte(1);
        out.writeUTF(internalName); // #2
        out.writeByte(7);
        out.writeShort(4); // #3 Class #4
        out.writeByte(1);
        out.writeUTF("java/lang/Object"); // #4
        out.writeShort(0x0021);
        out.writeShort(1);
        out.writeShort(3);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        out.writeShort(0); // methods
        out.writeShort(0); // attributes
        out.flush();
        return bos.toByteArray();
    }

    static MethodHandle classesGetter;

    static List<?> classes(ClassLoader loader) throws Throwable {
        return (List<?>) classesGetter.invoke(loader);
    }

    /** {@code list} holds exactly {@code expect}, by identity, in order. */
    static boolean same(List<?> list, List<Class<?>> expect) {
        if (list.size() != expect.size()) {
            return false;
        }
        for (int i = 0; i < expect.size(); i++) {
            if (list.get(i) != expect.get(i)) {
                return false;
            }
        }
        return true;
    }

    static void report(String step, ClassLoader loader, List<Class<?>> expect) throws Throwable {
        List<?> list = classes(loader);
        System.out.println(step + " size=" + list.size() + " same=" + same(list, expect));
    }

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 12;
        try {
            classesGetter = MethodHandles.privateLookupIn(ClassLoader.class, MethodHandles.lookup())
                    .findGetter(ClassLoader.class, "classes", ArrayList.class);
        } catch (ReflectiveOperationException | RuntimeException e) {
            System.out.println("classes unreadable: " + e.getClass().getName());
            return;
        }
        Ldr loader = new Ldr();
        List<Class<?>> defined = new ArrayList<>();
        report("fresh", loader, defined);

        for (int i = 0; i < n; i++) {
            defined.add(loader.def("Zt7ClA" + i, classBytes("Zt7ClA" + i)));
        }
        report("define1 n=" + n, loader, defined);

        defined.add(loader.defDirect("Zt7ClB", classBytes("Zt7ClB")));
        report("define2", loader, defined);

        try {
            loader.def("Zt7ClA0", classBytes("Zt7ClA0"));
            System.out.println("duplicate defined");
        } catch (LinkageError e) {
            report("duplicate " + e.getClass().getName(), loader, defined);
        }

        MethodHandles.Lookup host = MethodHandles.privateLookupIn(defined.get(0), MethodHandles.lookup());
        defined.add(host.defineClass(classBytes("Zt7ClC")));
        report("lookup", loader, defined);

        Class<?> hidden = host.defineHiddenClass(classBytes("Zt7ClH"), false).lookupClass();
        List<?> afterHidden = classes(loader);
        System.out.println("hidden size=" + afterHidden.size() + " same=" + same(afterHidden, defined)
                + " contains=" + afterHidden.contains(hidden));

        Ldr other = new Ldr();
        report("other", other, new ArrayList<>());

        // Optional (`app` as the second argument; a gap row, not the fix's):
        // the application loader's list, which on HotSpot holds every class it
        // defined (this probe's own classes) in definition order.
        if (args.length > 1 && args[1].equals("app")) {
            StringBuilder sb = new StringBuilder();
            for (Object c : classes(ClassLoader.getSystemClassLoader())) {
                sb.append(' ').append(((Class<?>) c).getName());
            }
            System.out.println("app classes:" + sb);
        }
    }
}
