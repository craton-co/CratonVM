// Interpreter round i4, wave 98, lane L5 -- jar manifest attribute NAMES are
// case-insensitive.
//
// `java.util.jar.Attributes.Name` compares ignoring ASCII case, and
// `JarFile.checkForSpecialAttributes` canonicalises the bytes it searches for
// (`\nCLASS-PATH: `, `\nMULTI-RELEASE: TRUE`), so on HotSpot a jar whose
// manifest says `class-path: dep/` and `multi-release: true` (lower case) gets
// its Class-Path directory searched and its `META-INF/versions/9/` entries
// served. CratonVM's `ManifestInfo::parse` (`classloading/src/class_path.rs`)
// matched the exact spellings `Class-Path` / `Multi-Release`, so the URL
// loader's class path (`--compatible`'s `ClassPath::for_url_loader` rows, the
// startup `-cp` jars in both modes) missed both.
//
// Self-contained: the probe writes its jar and directory under a fresh temp
// directory (manifest bytes written raw so the header case is exact).
//
// Run (no setup):
//   javac -d out L5W98ManifestHeaderCase.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W98ManifestHeaderCase
//
// Expected HotSpot 25 output (from the JDK sources; not measured by this lane):
// HOTSPOT_EXPECTED_BEGIN
// lower dep-resource=true
// lower dep-class=d.Dep same-loader=true
// lower versioned-resource=v9 url-versioned=true
// lower versioned-class-fields=1
// lower base-only=base-only
// upper dep-resource=true
// upper versioned-resource=v9
// HOTSPOT_EXPECTED_END

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W98ManifestHeaderCase {
    static byte[] cls(String name, boolean withField) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> {
            cb.withFlags(0x0021);
            cb.withSuperclass(ClassDesc.of("java.lang.Object"));
            if (withField) {
                cb.withField("v9", ClassDesc.of("java.lang.String"), 0x0009);
            }
        });
    }

    static void put(ZipOutputStream z, String name, byte[] bytes) throws IOException {
        z.putNextEntry(new ZipEntry(name));
        z.write(bytes);
        z.closeEntry();
    }

    static byte[] utf8(String s) {
        return s.getBytes(StandardCharsets.UTF_8);
    }

    static Path jar(Path dir, String file, String manifest) throws IOException {
        Path p = dir.resolve(file);
        try (OutputStream o = Files.newOutputStream(p); ZipOutputStream z = new ZipOutputStream(o)) {
            put(z, "META-INF/MANIFEST.MF", utf8(manifest));
            put(z, "r/base.txt", utf8("base"));
            put(z, "r/only.txt", utf8("base-only"));
            put(z, "META-INF/versions/9/r/base.txt", utf8("v9"));
            put(z, "m/V.class", cls("m.V", false));
            put(z, "META-INF/versions/9/m/V.class", cls("m.V", true));
        }
        return p;
    }

    static String read(ClassLoader l, String name) throws IOException {
        try (InputStream in = l.getResourceAsStream(name)) {
            if (in == null) {
                return "null";
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            in.transferTo(out);
            return out.toString(StandardCharsets.UTF_8);
        }
    }

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("l5w98mh").toRealPath();
        Path dep = root.resolve("dep");
        Files.createDirectories(dep.resolve("r"));
        Files.createDirectories(dep.resolve("d"));
        Files.write(dep.resolve("r/dep.txt"), utf8("dep"));
        Files.write(dep.resolve("d/Dep.class"), cls("d.Dep", false));

        Path lower = jar(root, "lower.jar",
                "Manifest-Version: 1.0\r\nclass-path: dep/\r\nmulti-release: true\r\n\r\n");
        try (URLClassLoader l = new URLClassLoader(new URL[] {lower.toUri().toURL()}, null)) {
            System.out.println("lower dep-resource=" + (l.getResource("r/dep.txt") != null));
            try {
                Class<?> c = l.loadClass("d.Dep");
                System.out.println("lower dep-class=" + c.getName() + " same-loader=" + (c.getClassLoader() == l));
            } catch (Throwable t) {
                System.out.println("lower dep-class=" + t.getClass().getName());
            }
            URL u = l.getResource("r/base.txt");
            System.out.println("lower versioned-resource=" + read(l, "r/base.txt")
                    + " url-versioned=" + (u != null && u.toString().endsWith("!/META-INF/versions/9/r/base.txt")));
            try {
                System.out.println("lower versioned-class-fields=" + l.loadClass("m.V").getDeclaredFields().length);
            } catch (Throwable t) {
                System.out.println("lower versioned-class-fields=" + t.getClass().getName());
            }
            System.out.println("lower base-only=" + read(l, "r/only.txt"));
        }

        Path upper = jar(root, "upper.jar",
                "Manifest-Version: 1.0\r\nCLASS-PATH: dep/\r\nMULTI-RELEASE: TRUE\r\n\r\n");
        try (URLClassLoader l = new URLClassLoader(new URL[] {upper.toUri().toURL()}, null)) {
            System.out.println("upper dep-resource=" + (l.getResource("r/dep.txt") != null));
            System.out.println("upper versioned-resource=" + read(l, "r/base.txt"));
        }
    }
}
