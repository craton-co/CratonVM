import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.jar.*;

/**
 * Interpreter round i5 wave 100, lane L3: a leading '/' in a URLClassLoader
 * resource name matches nothing on HotSpot (URLClassPath uses the name
 * verbatim: new URL(base, "/x") leaves the base directory and
 * JarFile.getEntry("/x") is null). CratonVM's findResource / findResources
 * natives stripped the slash and served "x", so getResources("/x") also
 * listed it.
 *
 * Run from the class directory (the probe writes l3w100slash/dir/s/x.txt and
 * l3w100slash/s.jar under the first class-path entry):
 *   java -cp . L3W100UrlLoaderLeadingSlash
 *
 * Expected HotSpot 25:
 *   dir findResource(/s/x.txt): null
 *   dir findResources(/s/x.txt): 0
 *   dir getResource(/s/x.txt): null
 *   dir getResources(/s/x.txt): 0
 *   dir getResourceAsStream(/s/x.txt): null
 *   dir findResource(s/x.txt): found
 *   dir getResources(s/x.txt): 1
 *   jar findResource(/s/x.txt): null
 *   jar findResources(/s/x.txt): 0
 *   jar getResources(/s/x.txt): 0
 *   jar findResource(s/x.txt): found
 *   jar getResources(s/x.txt): 1
 *   nullparent getResources(/java/lang/Object.class): 0
 */
public class L3W100UrlLoaderLeadingSlash {
    static String found(URL u) { return u == null ? "null" : "found"; }

    static int count(Enumeration<URL> e) { return Collections.list(e).size(); }

    static String read(InputStream in) throws IOException {
        if (in == null) return "null";
        try (InputStream s = in) {
            return new String(s.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    /** Exposes the protected findResource/findResources. */
    static final class Probe extends URLClassLoader {
        Probe(URL u) { super(new URL[] {u}, null); }
        URL find(String n) { return findResource(n); }
        Enumeration<URL> findAll(String n) throws IOException { return findResources(n); }
    }

    public static void main(String[] args) throws Exception {
        String first = System.getProperty("java.class.path").split(File.pathSeparator)[0];
        Path base = Paths.get(first).toAbsolutePath().resolve("l3w100slash");
        Path dir = base.resolve("dir");
        Files.createDirectories(dir.resolve("s"));
        Files.write(dir.resolve("s").resolve("x.txt"), "x".getBytes(StandardCharsets.UTF_8));
        Path jar = base.resolve("s.jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry("s/"));
            out.closeEntry();
            out.putNextEntry(new JarEntry("s/x.txt"));
            out.write("x".getBytes(StandardCharsets.UTF_8));
            out.closeEntry();
        }

        try (Probe d = new Probe(dir.toUri().toURL())) {
            System.out.println("dir findResource(/s/x.txt): " + found(d.find("/s/x.txt")));
            System.out.println("dir findResources(/s/x.txt): " + count(d.findAll("/s/x.txt")));
            System.out.println("dir getResource(/s/x.txt): " + found(d.getResource("/s/x.txt")));
            System.out.println("dir getResources(/s/x.txt): " + count(d.getResources("/s/x.txt")));
            System.out.println("dir getResourceAsStream(/s/x.txt): " + read(d.getResourceAsStream("/s/x.txt")));
            System.out.println("dir findResource(s/x.txt): " + found(d.find("s/x.txt")));
            System.out.println("dir getResources(s/x.txt): " + count(d.getResources("s/x.txt")));
        }
        try (Probe j = new Probe(jar.toUri().toURL())) {
            System.out.println("jar findResource(/s/x.txt): " + found(j.find("/s/x.txt")));
            System.out.println("jar findResources(/s/x.txt): " + count(j.findAll("/s/x.txt")));
            System.out.println("jar getResources(/s/x.txt): " + count(j.getResources("/s/x.txt")));
            System.out.println("jar findResource(s/x.txt): " + found(j.find("s/x.txt")));
            System.out.println("jar getResources(s/x.txt): " + count(j.getResources("s/x.txt")));
        }
        try (URLClassLoader n = new URLClassLoader(new URL[0], null)) {
            System.out.println("nullparent getResources(/java/lang/Object.class): "
                    + count(n.getResources("/java/lang/Object.class")));
        }
    }
}
