// Interpreter round i4, wave 98, lane L5 -- `ClassLoader.getResources`
// ordering and duplicates through `URLClassLoader` (`URLClassPath`):
//
//   * the URLs are searched in the order given; a jar's manifest `Class-Path`
//     URLs are pushed right after that jar (before the later given URLs);
//   * a URL given twice, or named again by a `Class-Path`, is opened once
//     (`URLClassPath.lmap` keyed by the URL's string) -- no duplicate answer;
//   * `getResources` asks the parent first (parent answers, then the child's);
//   * a missing name: empty enumeration, `getResource` null, `resources()` 0.
//
// Every answer prints with its root replaced by a label (d1, d2, j1, cp1, p1)
// so the output does not depend on the temp path; an answer under no known
// root prints `OTHER:<url>`.
//
// Self-contained: writes its directories and jar under a fresh temp directory.
//
// Run (no setup):
//   javac -d out L5W98ResourceOrder.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W98ResourceOrder
//
// Expected HotSpot 25 output (from the JDK sources; not measured by this lane):
// HOTSPOT_EXPECTED_BEGIN
// child-only: [d1, j1, cp1, d2]
// first: d1
// with-parent: [p1, d1, j1, cp1, d2]
// first-with-parent: p1
// jar-entry: [j1]
// cp-only: [cp1]
// missing: [] first=null stream=0
// dir-entry: [d1/, j1/, cp1/, d2/]
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.OutputStream;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.zip.ZipEntry;
import java.util.zip.ZipOutputStream;

public class L5W98ResourceOrder {
    static final Map<String, String> ROOTS = new LinkedHashMap<>();

    static void put(ZipOutputStream z, String name, String text) throws IOException {
        z.putNextEntry(new ZipEntry(name));
        if (text != null) {
            z.write(text.getBytes(StandardCharsets.UTF_8));
        }
        z.closeEntry();
    }

    static Path dir(Path root, String name, String... files) throws IOException {
        Path d = root.resolve(name);
        for (String f : files) {
            Path p = d.resolve(f);
            Files.createDirectories(p.getParent());
            Files.write(p, name.getBytes(StandardCharsets.UTF_8));
        }
        Files.createDirectories(d);
        return d;
    }

    static String label(URL u) {
        if (u == null) {
            return "null";
        }
        String s = u.toString();
        for (Map.Entry<String, String> e : ROOTS.entrySet()) {
            if (s.startsWith(e.getKey())) {
                String rest = s.substring(e.getKey().length());
                // keep a trailing-slash marker for directory answers
                return e.getValue() + (rest.endsWith("/") ? "/" : "");
            }
        }
        return "OTHER:" + s;
    }

    static List<String> labels(ClassLoader l, String name) throws IOException {
        List<String> out = new ArrayList<>();
        for (URL u : Collections.list(l.getResources(name))) {
            out.add(label(u));
        }
        return out;
    }

    public static void main(String[] args) throws Exception {
        Path root = Files.createTempDirectory("l5w98ro").toRealPath();
        Path d1 = dir(root, "d1", "r/x.txt");
        Path d2 = dir(root, "d2", "r/x.txt");
        Path cp1 = dir(root, "cp1", "r/x.txt", "r/cp.txt");
        Path p1 = dir(root, "p1", "r/x.txt");
        Path j1 = root.resolve("j1.jar");
        try (OutputStream o = Files.newOutputStream(j1); ZipOutputStream z = new ZipOutputStream(o)) {
            // cp1 named twice and d1 (already on the path) once more: one answer each
            put(z, "META-INF/MANIFEST.MF",
                    "Manifest-Version: 1.0\r\nClass-Path: cp1/ cp1/ d1/\r\n\r\n");
            put(z, "r/", null);
            put(z, "r/x.txt", "j1");
            put(z, "r/jar.txt", "j1");
        }
        ROOTS.put("jar:" + j1.toUri().toURL() + "!/", "j1");
        ROOTS.put(d1.toUri().toURL().toString(), "d1");
        ROOTS.put(d2.toUri().toURL().toString(), "d2");
        ROOTS.put(cp1.toUri().toURL().toString(), "cp1");
        ROOTS.put(p1.toUri().toURL().toString(), "p1");

        URL[] urls = {
            d1.toUri().toURL(), j1.toUri().toURL(), d2.toUri().toURL(), d1.toUri().toURL()
        };
        try (URLClassLoader child = new URLClassLoader(urls, null);
                URLClassLoader parent = new URLClassLoader(new URL[] {p1.toUri().toURL()}, null);
                URLClassLoader withParent = new URLClassLoader(urls, parent)) {
            System.out.println("child-only: " + labels(child, "r/x.txt"));
            System.out.println("first: " + label(child.getResource("r/x.txt")));
            System.out.println("with-parent: " + labels(withParent, "r/x.txt"));
            System.out.println("first-with-parent: " + label(withParent.getResource("r/x.txt")));
            System.out.println("jar-entry: " + labels(child, "r/jar.txt"));
            System.out.println("cp-only: " + labels(child, "r/cp.txt"));
            System.out.println("missing: " + labels(child, "r/none.txt")
                    + " first=" + label(child.getResource("r/none.txt"))
                    + " stream=" + child.resources("r/none.txt").count());
            System.out.println("dir-entry: " + labels(child, "r/"));
        }
    }
}
