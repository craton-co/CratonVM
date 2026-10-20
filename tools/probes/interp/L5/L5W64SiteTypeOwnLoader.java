// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 64, lane L5 follow-up -- a signature-polymorphic
// call site that names a class only a user loader can see
// (`docs/internal/fixed-bugs/i63-L5-the-spring-boot-fat-jar-no-longer-starts-under-jdk-only-FIXED-20261007.md`).
//
// JBoss Logging, inside a Spring Boot fat jar, builds its message logger with
// `lookup.findConstructor(Log_$logger, (Logger)void)` and calls it as
// `constructor.invoke(logger)`: the site's type `(Lorg/jboss/logging/Logger;)Object`
// is not the handle's, so `invoke` is `asType(siteType)` first, and the site's
// type is resolved with the CALLING class's loader (Spring's
// `LaunchedClassLoader`). CratonVM resolved it with `null` -- the system
// loader -- which cannot see the nested jar: `TypeNotPresentException: Type
// org.jboss.logging.Logger not present`, and Hibernate Validator's
// `ConfigurationImpl.<clinit>` failed with it.
//
// Here: the package-private classes below are copied into a temporary jar
// under package `pmh` (renamed by rewriting their constant pools' UTF-8
// entries, so the application loader does not see them) and loaded by a `URLClassLoader` whose parent is
// the platform loader. `pmh.User` runs the same shape -- `invoke` at a
// converting site, `invokeExact` at a wrong one, an exact `invokeExact` --
// plus the two lookups the page's step 1 names and a stack frame's
// `getMethodType()` (HotSpot resolves it with the declaring class's loader).
//
// Run (no setup; writes a temporary jar and deletes it):
//   javac -d out L5W64SiteTypeOwnLoader.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W64SiteTypeOwnLoader
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// forName=pmh.Own same-loader=true
// fromDescriptor=(Own)void
// invoke=pmh.Impl same-loader=true own=true
// invokeExact-wrong=java.lang.invoke.WrongMethodTypeException: handle's method type (Own)Impl but found (Own)Object
// invokeExact=pmh.Impl
// frameType=frameType(Own)String
// app-loader-sees-pmh=false
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.OutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.function.Supplier;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;

public class L5W64SiteTypeOwnLoader {
    private static final String[] COPIED = {"L5W64SiteOwn", "L5W64SiteImpl", "L5W64SiteUser", "L5W64SiteUser$Row"};
    private static final String[] FROM = {"L5W64SiteOwn", "L5W64SiteImpl", "L5W64SiteUser"};
    private static final String[] TO = {"pmh/Own", "pmh/Impl", "pmh/User"};

    /** `name` with each of FROM replaced by its TO. */
    private static String rename(String name) {
        for (int i = 0; i < FROM.length; i++) {
            name = name.replace(FROM[i], TO[i]);
        }
        return name;
    }

    /** The class file with every CONSTANT_Utf8 entry renamed; the rest verbatim. */
    private static byte[] renameClass(byte[] bytes) throws IOException {
        DataInputStream in = new DataInputStream(new java.io.ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> out.writeUTF(rename(in.readUTF()));
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                case 3, 4, 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                default -> throw new IOException("constant tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l5w64site");
        Path jar = dir.resolve("own.jar");
        try {
            try (OutputStream os = Files.newOutputStream(jar); JarOutputStream out = new JarOutputStream(os)) {
                for (int i = 0; i < COPIED.length; i++) {
                    byte[] bytes;
                    try (var in = L5W64SiteTypeOwnLoader.class.getClassLoader()
                            .getResourceAsStream(COPIED[i] + ".class")) {
                        bytes = in.readAllBytes();
                    }
                    out.putNextEntry(new ZipEntry(rename(COPIED[i]) + ".class"));
                    out.write(renameClass(bytes));
                    out.closeEntry();
                }
            }
            try (URLClassLoader loader = new URLClassLoader(
                    new URL[] {jar.toUri().toURL()}, ClassLoader.getPlatformClassLoader())) {
                var make = Class.forName("pmh.User", true, loader).getDeclaredConstructor();
                make.setAccessible(true); // a package-private class
                @SuppressWarnings("unchecked")
                Supplier<String> user = (Supplier<String>) make.newInstance();
                System.out.print(user.get());
            }
            String sees;
            try {
                Class.forName("pmh.Own", false, ClassLoader.getSystemClassLoader());
                sees = "true";
            } catch (ClassNotFoundException e) {
                sees = "false";
            }
            System.out.println("app-loader-sees-pmh=" + sees);
        } finally {
            Files.deleteIfExists(jar);
            Files.deleteIfExists(dir);
        }
    }
}

class L5W64SiteOwn {
    L5W64SiteOwn() {}
}

class L5W64SiteImpl {
    final L5W64SiteOwn own;

    L5W64SiteImpl(L5W64SiteOwn own) {
        this.own = own;
    }
}

class L5W64SiteUser implements Supplier<String> {
    public L5W64SiteUser() {}

    @Override
    public String get() {
        StringBuilder sb = new StringBuilder();
        ClassLoader mine = L5W64SiteUser.class.getClassLoader();
        sb.append("forName=").append(row(() -> {
            Class<?> c = Class.forName("pmh.Own", false, mine);
            return c.getName() + " same-loader=" + (c == L5W64SiteOwn.class);
        })).append('\n');
        sb.append("fromDescriptor=").append(row(() -> String.valueOf(
                java.lang.invoke.MethodType.fromMethodDescriptorString("(Lpmh/Own;)V", mine)))).append('\n');
        java.lang.invoke.MethodHandle mh;
        try {
            mh = java.lang.invoke.MethodHandles.lookup().findConstructor(L5W64SiteImpl.class,
                    java.lang.invoke.MethodType.methodType(void.class, L5W64SiteOwn.class));
        } catch (Throwable t) {
            return sb.append("findConstructor=").append(t).append('\n').toString();
        }
        final java.lang.invoke.MethodHandle h = mh;
        L5W64SiteOwn own = new L5W64SiteOwn();
        sb.append("invoke=").append(row(() -> {
            Object o = h.invoke(own);
            return o.getClass().getName() + " same-loader=" + (o.getClass().getClassLoader() == mine)
                    + " own=" + (((L5W64SiteImpl) o).own == own);
        })).append('\n');
        sb.append("invokeExact-wrong=").append(row(() -> {
            Object o = (Object) h.invokeExact(own);
            return "no exception: " + o.getClass().getName();
        })).append('\n');
        sb.append("invokeExact=").append(row(() -> {
            L5W64SiteImpl o = (L5W64SiteImpl) h.invokeExact(own);
            return o.getClass().getName();
        })).append('\n');
        sb.append("frameType=").append(row(() -> frameType(own))).append('\n');
        return sb.toString();
    }

    /** This frame's `getMethodType()`, which names `pmh.Own`. */
    private static String frameType(L5W64SiteOwn own) {
        return StackWalker.getInstance(StackWalker.Option.RETAIN_CLASS_REFERENCE)
                .walk(s -> s.findFirst().map(f -> f.getMethodName() + f.getMethodType()).orElse("none"));
    }

    interface Row {
        String run() throws Throwable;
    }

    private static String row(Row r) {
        try {
            return r.run();
        } catch (Throwable t) {
            return t.toString();
        }
    }
}
