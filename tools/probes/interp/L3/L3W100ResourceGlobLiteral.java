import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.jar.*;

/**
 * Interpreter round i5 wave 100, lane L3: a `*` or `?` in a resource name is
 * LITERAL on HotSpot (URLClassPath asks JarFile.getEntry(name) or
 * new File(dir, name)). CratonVM's class-path store treats it as a one-segment
 * glob, and the built-in loaders' doors answered the first glob match.
 *
 * Run from the class directory (it is the first class-path entry; the probe
 * writes l3w100glob/one.txt, l3w100glob/two.txt and l3w100glob.jar there):
 *   java -cp . L3W100ResourceGlobLiteral
 *
 * Expected HotSpot 25 (every glob row null / 0, every literal row found):
 *   app getResource(l3w100glob/*.txt): null
 *   app getResources(l3w100glob/*.txt): 0
 *   app getResourceAsStream(l3w100glob/*.txt): null
 *   app getResource(l3w100glob/?ne.txt): null
 *   system getSystemResource(l3w100glob/*.txt): null
 *   system getSystemResources(l3w100glob/*.txt): 0
 *   system getSystemResourceAsStream(l3w100glob/*.txt): null
 *   class getResource(/l3w100glob/*.txt): null
 *   class getResourceAsStream(/l3w100glob/*.txt): null
 *   jar getResource(q/*.txt): null
 *   jar getResources(q/*.txt): 0
 *   jar getResourceAsStream(q/*.txt): null
 *   literal app getResource(l3w100glob/one.txt): found
 *   literal app stream(l3w100glob/one.txt): one
 *   literal jar getResource(q/two.txt): found
 *   literal jar stream(q/two.txt): two
 */
public class L3W100ResourceGlobLiteral {
    static String found(URL u) { return u == null ? "null" : "found"; }

    static String read(InputStream in) throws IOException {
        if (in == null) return "null";
        try (InputStream s = in) {
            return new String(s.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    static int count(Enumeration<URL> e) { return Collections.list(e).size(); }

    public static void main(String[] args) throws Exception {
        String first = System.getProperty("java.class.path").split(File.pathSeparator)[0];
        Path cp = Paths.get(first).toAbsolutePath();
        Path dir = cp.resolve("l3w100glob");
        Files.createDirectories(dir);
        Files.write(dir.resolve("one.txt"), "one".getBytes(StandardCharsets.UTF_8));
        Files.write(dir.resolve("two.txt"), "two".getBytes(StandardCharsets.UTF_8));
        Path jar = cp.resolve("l3w100glob.jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry("q/"));
            out.closeEntry();
            out.putNextEntry(new JarEntry("q/two.txt"));
            out.write("two".getBytes(StandardCharsets.UTF_8));
            out.closeEntry();
        }

        ClassLoader app = ClassLoader.getSystemClassLoader();
        String g = "l3w100glob/*.txt";
        System.out.println("app getResource(" + g + "): " + found(app.getResource(g)));
        System.out.println("app getResources(" + g + "): " + count(app.getResources(g)));
        System.out.println("app getResourceAsStream(" + g + "): " + read(app.getResourceAsStream(g)));
        System.out.println("app getResource(l3w100glob/?ne.txt): " + found(app.getResource("l3w100glob/?ne.txt")));
        System.out.println("system getSystemResource(" + g + "): " + found(ClassLoader.getSystemResource(g)));
        System.out.println("system getSystemResources(" + g + "): " + count(ClassLoader.getSystemResources(g)));
        System.out.println("system getSystemResourceAsStream(" + g + "): " + read(ClassLoader.getSystemResourceAsStream(g)));
        Class<?> c = L3W100ResourceGlobLiteral.class;
        System.out.println("class getResource(/" + g + "): " + found(c.getResource("/" + g)));
        System.out.println("class getResourceAsStream(/" + g + "): " + read(c.getResourceAsStream("/" + g)));

        try (URLClassLoader ucl = new URLClassLoader(new URL[] {jar.toUri().toURL()}, null)) {
            System.out.println("jar getResource(q/*.txt): " + found(ucl.getResource("q/*.txt")));
            System.out.println("jar getResources(q/*.txt): " + count(ucl.getResources("q/*.txt")));
            System.out.println("jar getResourceAsStream(q/*.txt): " + read(ucl.getResourceAsStream("q/*.txt")));
            System.out.println("literal app getResource(l3w100glob/one.txt): " + found(app.getResource("l3w100glob/one.txt")));
            System.out.println("literal app stream(l3w100glob/one.txt): " + read(app.getResourceAsStream("l3w100glob/one.txt")));
            System.out.println("literal jar getResource(q/two.txt): " + found(ucl.getResource("q/two.txt")));
            System.out.println("literal jar stream(q/two.txt): " + read(ucl.getResourceAsStream("q/two.txt")));
        }
    }
}
