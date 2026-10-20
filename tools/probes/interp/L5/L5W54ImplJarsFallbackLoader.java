// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 54, lane L5 -- a class a loader could not find is
// never defined into it behind its back. CratonVM's IMPL-JARS fallback (written
// for Elasticsearch's `EmbeddedImplClassLoader`) ran after EVERY failed
// `loadClass` -- the VM-driven resolution door (`constants.rs`
// `drive_defining_loader_load_named`), the base `loadClass` native and the
// base `findClass(String, String)` native the application loader uses: for a
// `com.fasterxml.jackson.*` / `org.yaml.snakeyaml.*` / `org.elasticsearch.*`
// name it searched the class path for `IMPL-JARS/<module>/LISTING.TXT` and,
// when a listed directory held the class, defined it into the loader that had
// just refused it
// (`docs/internal/fixed-bugs/interpreter-L5-a-failed-loader-drive-searches-the-class-path-for-elasticsearch-impl-jars-FIXED-20261018.md`).
//
// The probe writes an IMPL-JARS layout into its own class-path directory
// (`IMPL-JARS/x-content/LISTING.TXT` naming `l5w54-fake.jar`, and
// `IMPL-JARS/x-content/l5w54-fake.jar/com/fasterxml/jackson/l5w54/Hidden.class`),
// which HotSpot ignores, then (rows first on a cold VM, and again last):
//   app-forName  `Class.forName` of the hidden name from the application loader
//   user-load    `loadClass` of it on a null-parent loader whose `findClass`
//                refuses it (`asked` = how often the loader was asked for it)
//   user-ref     a class that loader defines, `ldc` of the hidden class: the
//                resolution's error and its cause
// The fixture is removed at the end.
//
// Run (no setup; the class-path entry must be a writable directory):
//   javac -d out L5W54ImplJarsFallbackLoader.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W54ImplJarsFallbackLoader
//
// Positive control: `CRATONVM_IMPL_JARS_ES_LOADER_ONLY=0` restores the
// fallback for every loader. Measured on the base (a 2026-09-25 release build,
// Windows, both modes): `user-ref-1=found loader=user asked=1` and
// `user-ref-2=found loader=user asked=1` -- the resolution defined the hidden
// class into the loader that had just refused it; every other row matched.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   fixture=written
//   app-forName-1=java.lang.ClassNotFoundException
//   user-load-1=java.lang.ClassNotFoundException asked=1
//   user-ref-1=java.lang.NoClassDefFoundError com/fasterxml/jackson/l5w54/Hidden cause=java.lang.ClassNotFoundException asked=1
//   app-forName-2=java.lang.ClassNotFoundException
//   user-load-2=java.lang.ClassNotFoundException asked=1
//   user-ref-2=java.lang.NoClassDefFoundError com/fasterxml/jackson/l5w54/Hidden cause=java.lang.ClassNotFoundException asked=1
//   fixture=removed

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.File;
import java.io.IOException;
import java.lang.reflect.InvocationTargetException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Comparator;
import java.util.List;
import java.util.stream.Stream;

public class L5W54ImplJarsFallbackLoader {
    static final String HIDDEN = "com.fasterxml.jackson.l5w54.Hidden";
    static final String HIDDEN_INTERNAL = "com/fasterxml/jackson/l5w54/Hidden";

    /** A class file: `public class <name> extends Object`, optionally with
     *  `public static Object get() { return <ref>.class; }` (version 49: no
     *  stack map needed). */
    static byte[] classFile(String name, String ref) throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(ref == null ? 5 : 10);
        out.writeByte(1); out.writeUTF(name);                 // #1
        out.writeByte(7); out.writeShort(1);                  // #2 this
        out.writeByte(1); out.writeUTF("java/lang/Object");   // #3
        out.writeByte(7); out.writeShort(3);                  // #4 super
        if (ref != null) {
            out.writeByte(1); out.writeUTF(ref);                     // #5
            out.writeByte(7); out.writeShort(5);                     // #6
            out.writeByte(1); out.writeUTF("get");                   // #7
            out.writeByte(1); out.writeUTF("()Ljava/lang/Object;");  // #8
            out.writeByte(1); out.writeUTF("Code");                  // #9
        }
        out.writeShort(0x21);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0); // interfaces
        out.writeShort(0); // fields
        if (ref == null) {
            out.writeShort(0);
        } else {
            out.writeShort(1);
            out.writeShort(0x09); // public static
            out.writeShort(7);
            out.writeShort(8);
            out.writeShort(1);
            out.writeShort(9);
            out.writeInt(2 + 2 + 4 + 3 + 2 + 2);
            out.writeShort(1); // max_stack
            out.writeShort(0); // max_locals
            out.writeInt(3);
            out.writeByte(0x12); out.writeByte(6); // ldc #6
            out.writeByte(0xB0);                   // areturn
            out.writeShort(0); // exception table
            out.writeShort(0); // code attributes
        }
        out.writeShort(0); // class attributes
        out.flush();
        return bytes.toByteArray();
    }

    static final class Refuser extends ClassLoader {
        final byte[] ref;
        int asked;

        Refuser(byte[] ref) {
            super(null);
            this.ref = ref;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals("l5w54.Ref")) {
                return defineClass(name, ref, 0, ref.length);
            }
            if (name.equals(HIDDEN)) {
                asked++;
            }
            throw new ClassNotFoundException(name);
        }
    }

    static String where(Class<?> c) {
        ClassLoader l = c.getClassLoader();
        if (l == null) {
            return "boot";
        }
        if (l == ClassLoader.getSystemClassLoader()) {
            return "app";
        }
        return l instanceof Refuser ? "user" : l.getClass().getName();
    }

    static void round(int n, byte[] ref) {
        try {
            Class<?> c = Class.forName(HIDDEN);
            System.out.println("app-forName-" + n + "=found loader=" + where(c));
        } catch (Throwable t) {
            System.out.println("app-forName-" + n + "=" + t.getClass().getName());
        }
        Refuser direct = new Refuser(ref);
        try {
            Class<?> c = direct.loadClass(HIDDEN);
            System.out.println("user-load-" + n + "=found loader=" + where(c) + " asked=" + direct.asked);
        } catch (Throwable t) {
            System.out.println("user-load-" + n + "=" + t.getClass().getName() + " asked=" + direct.asked);
        }
        Refuser viaRef = new Refuser(ref);
        try {
            Class<?> r = viaRef.loadClass("l5w54.Ref");
            Object got = r.getMethod("get").invoke(null);
            System.out.println("user-ref-" + n + "=found loader=" + where((Class<?>) got)
                    + " asked=" + viaRef.asked);
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            Throwable cause = t.getCause();
            System.out.println("user-ref-" + n + "=" + t.getClass().getName() + " " + t.getMessage()
                    + " cause=" + (cause == null ? "none" : cause.getClass().getName())
                    + " asked=" + viaRef.asked);
        } catch (Throwable t) {
            System.out.println("user-ref-" + n + "=outer " + t.getClass().getName());
        }
    }

    static Path classPathDirectory() {
        for (String entry : System.getProperty("java.class.path").split(File.pathSeparator)) {
            Path p = Path.of(entry).toAbsolutePath();
            if (Files.isRegularFile(p.resolve("L5W54ImplJarsFallbackLoader.class"))) {
                return p;
            }
        }
        return null;
    }

    public static void main(String[] args) throws Exception {
        Path dir = classPathDirectory();
        if (dir == null) {
            System.out.println("fixture=skipped (the class-path entry is not a directory)");
            return;
        }
        Path root = dir.resolve("IMPL-JARS");
        Path module = root.resolve("x-content");
        Path hidden = module.resolve("l5w54-fake.jar").resolve(HIDDEN_INTERNAL + ".class");
        Files.createDirectories(hidden.getParent());
        Files.write(module.resolve("LISTING.TXT"), "l5w54-fake.jar\n".getBytes("UTF-8"));
        Files.write(hidden, classFile(HIDDEN_INTERNAL, null));
        System.out.println("fixture=written");
        byte[] ref = classFile("l5w54/Ref", HIDDEN_INTERNAL);
        try {
            round(1, ref);
            round(2, ref);
        } finally {
            List<Path> all = new ArrayList<>();
            try (Stream<Path> s = Files.walk(root)) {
                s.sorted(Comparator.reverseOrder()).forEach(all::add);
            }
            for (Path p : all) {
                Files.deleteIfExists(p);
            }
            System.out.println("fixture=removed");
        }
    }
}
