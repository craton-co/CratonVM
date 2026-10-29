// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.ObjectInputStream;
import java.io.ObjectOutputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardCopyOption;
import java.nio.file.attribute.FileTime;
import java.util.Locale;
import java.util.function.Function;

/**
 * Regression: three runtime defects the Quarkus suite found (2026-10-09), each
 * of which killed an application before it served a request.
 *
 * <ol>
 * <li>{@code java.io.File} is serializable only if {@code File.<clinit>} has
 * set the three {@code Unsafe} field offsets {@code readObject} uses; a
 * native replacement of the initializer that skipped them made every
 * deserialized {@code File} throw (Quarkus bootstrap serializes its models).
 * <li>{@code Files.copy(.., COPY_ATTRIBUTES)} on Windows reaches the token and
 * privilege natives of {@code WindowsNativeDispatcher}
 * ({@code GetCurrentProcess} and five more); the Quarkus fast-jar builder
 * died with {@code UnsatisfiedLinkError}. On other hosts the copy never
 * reaches them, which is fine: the line is the same everywhere.
 * <li>A boxed {@code Boolean} arriving at a LambdaForm {@code boolean}/{@code int}
 * slot ("Cannot cast Boolean to Integer"); every Groovy call site that passes
 * a boolean takes this shape.
 * <li>A lambda of a sub-interface that implements {@code Function.apply} with a
 * default method, called through {@code Function.apply} from compiled code:
 * the by-name tail dispatched against the site's class and the abstract
 * {@code Function.apply} answered its first argument (AssertJ
 * {@code extracting(..)}; wave 1 of the Quarkus suite).
 * <li>{@code Locale.forLanguageTag} on an ill-formed tag answers
 * {@code Locale.ROOT} itself ({@code LocaleConverter} is bytecode again).
 * <li>A class loader that declares its own {@code name} field (Quarkus's
 * {@code QuarkusClassLoader}) must not shadow {@code ClassLoader.name}: both
 * {@code getName()} and the loader name printed in a stack frame read the
 * field {@code java.lang.ClassLoader} declares, which {@code super(parent)}
 * leaves null.
 * </ol>
 *
 * HotSpot is the oracle: every line is diffed against it.
 */
public class RQuarkusRuntime {

    static int checks = 0;

    static void ck(String key, Object value) {
        checks++;
        System.out.println("CK RQuarkusRuntime " + key + "=" + value);
    }

    interface Throwing<F, R> extends Function<F, R> {
        R extractThrows(F f) throws Throwable;

        default R apply(F f) {
            try {
                return extractThrows(f);
            } catch (RuntimeException e) {
                throw e;
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        }
    }

    static Object viaFunction(Function<Object, Object> f, Object x) {
        return f.apply(x);
    }

    static final class ShadowLoader extends ClassLoader {
        @SuppressWarnings("unused")
        private final String name = "Shadow Name";

        ShadowLoader() {
            super((ClassLoader) null);
        }

        Class<?> define(byte[] bytes) {
            return defineClass(null, bytes, 0, bytes.length);
        }
    }

    public static final class Probe {
        public static String frame() {
            StackTraceElement e = new Throwable().getStackTrace()[0];
            return e.getClassLoaderName() + "|" + e.toString().startsWith("Shadow");
        }
    }

    static boolean flag(boolean b) {
        return !b;
    }

    static int pick(boolean a, int x, boolean b) {
        return (a ? x : -x) + (b ? 1000 : 0);
    }

    static File roundTrip(File f) throws Exception {
        ByteArrayOutputStream bos = new ByteArrayOutputStream();
        try (ObjectOutputStream out = new ObjectOutputStream(bos)) {
            out.writeObject(f);
        }
        try (ObjectInputStream in = new ObjectInputStream(new ByteArrayInputStream(bos.toByteArray()))) {
            return (File) in.readObject();
        }
    }

    public static void main(String[] args) throws Throwable {
        // 1. File serialization.
        for (String p : new String[] { "plain.txt", "dir" + File.separator + "child file.txt", "" }) {
            File f = new File(p);
            File g = roundTrip(f);
            ck("file[" + p + "]", "path=[" + g.getPath() + "] equals=" + f.equals(g) + " name=[" + g.getName() + "]");
        }
        File abs = new File(System.getProperty("java.io.tmpdir"), "a" + File.separator + "b.txt").getAbsoluteFile();
        File absBack = roundTrip(abs);
        ck("file.abs", "equals=" + abs.equals(absBack) + " absolute=" + absBack.isAbsolute());

        // 2. Files.copy with COPY_ATTRIBUTES.
        Path dir = Files.createTempDirectory("rquarkus");
        try {
            Path src = dir.resolve("src.txt");
            Files.writeString(src, "attributes");
            FileTime stamp = FileTime.fromMillis(1_700_000_000_000L);
            Files.setLastModifiedTime(src, stamp);
            Path dst = dir.resolve("dst.txt");
            Files.copy(src, dst, StandardCopyOption.COPY_ATTRIBUTES);
            ck("copy", "content=" + Files.readString(dst)
                    + " mtimePreserved=" + stamp.equals(Files.getLastModifiedTime(dst)));
            Files.copy(src, dst, StandardCopyOption.COPY_ATTRIBUTES, StandardCopyOption.REPLACE_EXISTING);
            ck("copy.replace", "mtimePreserved=" + stamp.equals(Files.getLastModifiedTime(dst)));
        } finally {
            try (var s = Files.walk(dir)) {
                s.sorted(java.util.Comparator.reverseOrder()).forEach(q -> q.toFile().delete());
            }
        }

        // 3. Boxed Boolean through a method handle.
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodHandle flag = lookup.findStatic(RQuarkusRuntime.class, "flag", MethodType.methodType(boolean.class, boolean.class));
        MethodHandle generic = flag.asType(MethodType.methodType(Object.class, Object.class));
        ck("boolean.generic", generic.invoke((Object) Boolean.TRUE) + "," + generic.invoke((Object) Boolean.FALSE));
        MethodHandle pick = lookup.findStatic(RQuarkusRuntime.class, "pick",
                MethodType.methodType(int.class, boolean.class, int.class, boolean.class));
        MethodHandle spread = pick.asType(MethodType.methodType(Object.class, Object.class, Object.class, Object.class));
        ck("pick.spread", spread.invoke((Object) Boolean.TRUE, (Object) 7, (Object) Boolean.TRUE)
                + "," + spread.invoke((Object) Boolean.FALSE, (Object) 7, (Object) Boolean.FALSE));
        ck("pick.invokeWithArguments", pick.invokeWithArguments(Boolean.FALSE, 5, Boolean.TRUE));
        // 4. Lambda of a sub-interface, default apply, hot enough to be compiled.
        Throwing<Object, Object> mapper = t -> "mapped:" + t;
        int wrong = 0;
        for (int i = 0; i < 5000; i++) {
            if (!("mapped:" + i).equals(viaFunction(mapper, i))) {
                wrong++;
            }
        }
        ck("lambda.defaultApply", "wrong=" + wrong);

        // 5. Locale.forLanguageTag.
        ck("locale.ill-formed", Locale.forLanguageTag("c.u-US") == Locale.ROOT);
        ck("locale.valid", Locale.forLanguageTag("en-US"));
        // 6. A loader's own `name` field must not shadow ClassLoader.name.
        byte[] probeBytes;
        try (var in = RQuarkusRuntime.class.getResourceAsStream("RQuarkusRuntime$Probe.class")) {
            probeBytes = in.readAllBytes();
        }
        ShadowLoader shadow = new ShadowLoader();
        Class<?> probe = shadow.define(probeBytes);
        ck("loader.getName", shadow.getName());
        ck("loader.frame", probe.getMethod("frame").invoke(null));
        System.out.println("CK RQuarkusRuntime checks=" + checks);
        System.out.println("PASS RQuarkusRuntime (" + checks + " checks)");
    }
}
