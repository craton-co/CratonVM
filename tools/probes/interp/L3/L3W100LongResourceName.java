import java.io.*;
import java.net.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;

/**
 * Interpreter round i5 wave 100, lane L3: a resource whose name is longer
 * than 256 chars. HotSpot bounds no resource-name length: getResource finds
 * it and getResourceAsStream opens it. CratonVM's ClassLoader stream door
 * validated the name with a 256-byte bound first, so the stream was null
 * while getResource found the same resource.
 *
 * Run from the class directory (the first class-path entry; the probe writes
 * l3w100long/<250 x 'a'>.txt there):
 *   java -cp . L3W100LongResourceName
 *
 * Expected HotSpot 25:
 *   name length: 265
 *   app getResource: found
 *   app getResourceAsStream: long
 *   context getResourceAsStream: long
 *   class getResourceAsStream: long
 *   system getSystemResourceAsStream: long
 *   short control getResourceAsStream: short
 */
public class L3W100LongResourceName {
    static String read(InputStream in) throws IOException {
        if (in == null) return "null";
        try (InputStream s = in) {
            return new String(s.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    public static void main(String[] args) throws Exception {
        String first = System.getProperty("java.class.path").split(File.pathSeparator)[0];
        Path cp = Paths.get(first).toAbsolutePath();
        String leaf = "a".repeat(250) + ".txt";
        String name = "l3w100long/" + leaf;
        Path dir = cp.resolve("l3w100long");
        Files.createDirectories(dir);
        Files.write(dir.resolve(leaf), "long".getBytes(StandardCharsets.UTF_8));
        Files.write(dir.resolve("short.txt"), "short".getBytes(StandardCharsets.UTF_8));

        ClassLoader app = ClassLoader.getSystemClassLoader();
        ClassLoader ctx = Thread.currentThread().getContextClassLoader();
        System.out.println("name length: " + name.length());
        System.out.println("app getResource: " + (app.getResource(name) == null ? "null" : "found"));
        System.out.println("app getResourceAsStream: " + read(app.getResourceAsStream(name)));
        System.out.println("context getResourceAsStream: " + read(ctx.getResourceAsStream(name)));
        System.out.println("class getResourceAsStream: "
                + read(L3W100LongResourceName.class.getResourceAsStream("/" + name)));
        System.out.println("system getSystemResourceAsStream: " + read(ClassLoader.getSystemResourceAsStream(name)));
        System.out.println("short control getResourceAsStream: "
                + read(app.getResourceAsStream("l3w100long/short.txt")));
    }
}
