// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.URI;
import java.nio.file.*;
import java.nio.file.spi.FileSystemProvider;
import java.util.*;

/**
 * Which providers serve {@code jar:} and {@code jrt:}, and do they behave like HotSpot's.
 *
 * <p>Usage: {@code JarJrtProviderProbe <h2-2.4.240.jar>} (it reads {@code org/h2} entries). Every row must print what HotSpot prints:
 * the installed providers are the real {@code jdk.nio.zipfs} / {@code jdk.internal.jrtfs}
 * ones, a mounted archive is a {@code ZipFileSystem} of {@code ZipPath}s, an unmounted
 * {@code jar:} URI is {@code FileSystemNotFoundException}, a non-archive is
 * {@code ProviderNotFoundException}, and a class of the runtime image has a null code source.
 * {@code docs/internal/fixed-suite-bugs/hibernate/h2-inprocess-javac-jrt-modules-listing-gap-FIXED-20260923.md}.
 */
public class JarJrtProviderProbe {
    static void row(String k, Object v) { System.out.println(k + " = " + v); }
    public static void main(String[] a) throws Exception {
        Path jar = Paths.get(a[0]);
        List<String> schemes = new ArrayList<>();
        for (FileSystemProvider p : FileSystemProvider.installedProviders()) schemes.add(p.getScheme() + ":" + p.getClass().getName());
        row("installed", schemes);
        row("installed[0] == default provider", FileSystemProvider.installedProviders().get(0) == FileSystems.getDefault().provider());
        URI u = URI.create("jar:" + jar.toUri() + "!/");
        try { Paths.get(URI.create("jar:" + jar.toUri() + "!/org/h2/Driver.class")); row("unmounted Paths.get", "no throw"); }
        catch (FileSystemNotFoundException e) { row("unmounted Paths.get", "FileSystemNotFoundException"); }
        try (FileSystem fs = FileSystems.newFileSystem(u, Map.of())) {
            row("uri fs class", fs.getClass().getName());
            Path p = Paths.get(URI.create("jar:" + jar.toUri() + "!/org/h2/Driver.class"));
            row("Paths.get(jar uri) class", p.getClass().getName());
            row("Paths.get(jar uri) fs == mounted", p.getFileSystem() == fs);
            row("Driver.class exists", Files.exists(p));
            row("toUri roundtrip", p.toUri().toString().endsWith("!/org/h2/Driver.class"));
            try { FileSystems.newFileSystem(u, Map.of()); row("second mount", "no throw"); }
            catch (FileSystemAlreadyExistsException e) { row("second mount", "FileSystemAlreadyExistsException"); }
            row("getFileSystem(uri) == mounted", FileSystems.getFileSystem(u) == fs);
        }
        try (FileSystem fs = FileSystems.newFileSystem(jar)) {
            row("path fs class", fs.getClass().getName());
            row("path fs provider", fs.provider().getClass().getName());
            Path root = fs.getRootDirectories().iterator().next();
            row("root class", root.getClass().getName());
            row("sep", fs.getSeparator());
            int[] n = {0};
            Files.walkFileTree(root, new SimpleFileVisitor<Path>() {
                public FileVisitResult visitFile(Path f, java.nio.file.attribute.BasicFileAttributes at) { n[0]++; return FileVisitResult.CONTINUE; }
            });
            row("files", n[0]);
            Path d = fs.getPath("/org/h2/tools");
            row("relativize", root.relativize(d));
            row("getFileName", d.getFileName());
        }
        Path notZip = Files.createTempFile("probe", ".txt");
        Files.writeString(notZip, "hello");
        try { FileSystems.newFileSystem(notZip); row("non-archive", "no throw"); }
        catch (ProviderNotFoundException e) { row("non-archive", "ProviderNotFoundException"); }
        catch (Exception e) { row("non-archive", e.getClass().getName()); }
        Path fresh = Files.createTempDirectory("zprobe").resolve("new.zip");
        try (FileSystem fs = FileSystems.newFileSystem(fresh, Map.of("create", "true"))) {
            Files.writeString(fs.getPath("/a.txt"), "zip-content");
        }
        try (FileSystem fs = FileSystems.newFileSystem(fresh)) {
            row("created zip read back", Files.readString(fs.getPath("/a.txt")));
        }
        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        row("jrt fs class", jrt.getClass().getName());
        row("jrt Object.class bytes", Files.readAllBytes(jrt.getPath("/modules/java.base/java/lang/Object.class")).length);
        row("jrt Paths.get", Paths.get(URI.create("jrt:/java.base/java/lang/Object.class")).getClass().getName());
        row("SystemImage-like PD codesource null", Object.class.getProtectionDomain().getCodeSource() == null);
        row("app PD codesource non-null", JarJrtProviderProbe.class.getProtectionDomain().getCodeSource() != null);
        System.out.println("PROBE_DONE");
    }
}
