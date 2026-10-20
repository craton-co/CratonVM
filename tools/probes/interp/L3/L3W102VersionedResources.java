import java.io.*;
import java.lang.module.Configuration;
import java.lang.module.ModuleFinder;
import java.net.URL;
import java.net.URLClassLoader;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.jar.*;
import java.util.stream.Collectors;
import java.util.zip.ZipFile;

/**
 * Interpreter round i5 wave 102, lane L3 (hunt): resources of a Multi-Release
 * jar, on the class path (URLClassLoader, JarFile with and without the
 * runtime version) and on the module path (an automatic module in a child
 * layer: Module.getResourceAsStream, the layer loader's getResource /
 * getResources), versioned entries 9, 11, 17 and 99 (99 is above the runtime
 * and must be ignored), a resource present only in a versioned directory, and
 * a non-Multi-Release jar that has a META-INF/versions directory (never
 * versioned). URLs print only the part after the jar separator (`!/`), with
 * any fragment, so the temp path does not matter.
 *
 * Run: java -cp . L3W102VersionedResources (both modes). Expected: whatever
 * HotSpot 25 prints.
 */
public class L3W102VersionedResources {
    static void put(JarOutputStream out, String name, String body) throws IOException {
        out.putNextEntry(new JarEntry(name));
        out.write(body.getBytes(StandardCharsets.UTF_8));
        out.closeEntry();
    }

    static Path jar(Path dir, String file, boolean multiRelease, String pkg) throws IOException {
        Path jar = dir.resolve(file);
        Manifest mf = new Manifest();
        mf.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        if (multiRelease) mf.getMainAttributes().put(new Attributes.Name("Multi-Release"), "true");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar), mf)) {
            put(out, "res.txt", "base");
            put(out, "META-INF/versions/9/res.txt", "v9");
            put(out, "META-INF/versions/11/res.txt", "v11");
            put(out, "META-INF/versions/11/only11.txt", "only11");
            put(out, "META-INF/versions/99/res.txt", "v99");
            put(out, "META-INF/versions/99/only99.txt", "only99");
            put(out, pkg + "/a.txt", "a");
            put(out, "META-INF/versions/17/" + pkg + "/a.txt", "a17");
            put(out, "META-INF/versions/17/" + pkg + "/b17.txt", "b17");
        }
        return jar;
    }

    static String read(InputStream in) throws IOException {
        if (in == null) return "null";
        try (in) {
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    static String tail(URL u) {
        if (u == null) return "null";
        String s = u.toString();
        int i = s.indexOf("!/");
        return (s.startsWith("jar:") ? "jar:" : s.substring(0, Math.min(5, s.length())) + "..")
                + (i < 0 ? "?" : s.substring(i));
    }

    static String all(Enumeration<URL> e) {
        List<String> out = new ArrayList<>();
        while (e.hasMoreElements()) out.add(tail(e.nextElement()));
        return out.toString();
    }

    static final String[] NAMES = {
        "res.txt", "only11.txt", "only99.txt", "p/a.txt", "p/b17.txt",
        "META-INF/versions/9/res.txt", "META-INF/versions/99/res.txt", "/res.txt",
    };

    public static void main(String[] args) throws Exception {
        Path dir = Files.createTempDirectory("l3w102mr");
        Path mr = jar(dir, "mr.jar", true, "p");
        Path plain = jar(dir, "plain.jar", false, "p");

        for (Path j : List.of(mr, plain)) {
            String tag = j.getFileName().toString();
            try (URLClassLoader l = new URLClassLoader(new URL[] {j.toUri().toURL()}, null)) {
                for (String n : NAMES) {
                    System.out.println("cp " + tag + " " + n + ": " + read(l.getResourceAsStream(n))
                            + " url=" + tail(l.getResource(n)) + " all=" + all(l.getResources(n)));
                }
                // The URL's own stream (JarURLConnection, `#runtime` or not).
                URL u = l.getResource("res.txt");
                System.out.println("cp " + tag + " url-stream res.txt: " + (u == null ? "null" : read(u.openStream())));
            }
            try (JarFile jf = new JarFile(j.toFile(), true, ZipFile.OPEN_READ, Runtime.version())) {
                System.out.println("jarfile " + tag + " isMultiRelease=" + jf.isMultiRelease()
                        + " version=" + (jf.getVersion().feature() == Runtime.version().feature() ? "runtime" : String.valueOf(jf.getVersion())));
                for (String n : new String[] {"res.txt", "only11.txt", "only99.txt", "p/a.txt", "p/b17.txt"}) {
                    JarEntry e = jf.getJarEntry(n);
                    System.out.println("jarfile " + tag + " " + n + ": "
                            + (e == null ? "null" : e.getName() + " real=" + e.getRealName() + " body=" + read(jf.getInputStream(e))));
                }
                System.out.println("jarfile " + tag + " versionedStream: " + jf.versionedStream()
                        .map(JarEntry::getName).filter(n -> !n.startsWith("META-INF/MANIFEST"))
                        .sorted().collect(Collectors.toList()));
            }
            try (JarFile jf = new JarFile(j.toFile())) {
                JarEntry e = jf.getJarEntry("res.txt");
                System.out.println("jarfile-base " + tag + " res.txt: " + e.getRealName() + " body=" + read(jf.getInputStream(e)));
            }
        }

        // Module path: an automatic module named from the jar file.
        Path modDir = Files.createDirectories(dir.resolve("mods"));
        Path modJar = jar(modDir, "autom.jar", true, "q");
        ModuleFinder finder = ModuleFinder.of(modJar);
        Configuration cf = ModuleLayer.boot().configuration()
                .resolve(finder, ModuleFinder.of(), Set.of("autom"));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf, ClassLoader.getSystemClassLoader());
        Module m = layer.findModule("autom").orElseThrow();
        ClassLoader ml = layer.findLoader("autom");
        System.out.println("mod packages: " + new TreeSet<>(m.getPackages()));
        for (String n : new String[] {"q/a.txt", "q/b17.txt", "res.txt", "only11.txt", "META-INF/MANIFEST.MF", "/q/a.txt"}) {
            String viaModule;
            try {
                viaModule = read(m.getResourceAsStream(n));
            } catch (IOException ex) {
                viaModule = ex.getClass().getName();
            }
            String body = n.equals("META-INF/MANIFEST.MF") ? (viaModule.equals("null") ? "null" : "present") : viaModule;
            String loaderBody = read(ml.getResourceAsStream(n));
            if (n.equals("META-INF/MANIFEST.MF") && !loaderBody.equals("null")) loaderBody = "present";
            System.out.println("mod " + n + ": module=" + body + " loader=" + loaderBody
                    + " url=" + tail(ml.getResource(n)) + " all=" + all(ml.getResources(n)));
        }
    }
}
