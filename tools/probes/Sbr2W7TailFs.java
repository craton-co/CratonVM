import java.io.*;
import java.net.URI;
import java.nio.file.*;
import java.nio.file.spi.FileSystemProvider;
import java.util.*;
import java.util.zip.*;

/** SBR2 W7 TAIL: FileSystems.getDefault identity against installedProviders / Path / zip providers. */
public class Sbr2W7TailFs {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    static String ex(Throwable t) { return t.getClass().getName() + ": " + t.getMessage(); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + ex(e)); } }

    public static void main(String[] a) throws Throwable {
        FileSystem d = FileSystems.getDefault();
        t("default class kind", () -> d.getClass().getSimpleName().endsWith("FileSystem"));
        t("installed[0] is default provider", () -> FileSystemProvider.installedProviders().get(0) == d.provider());
        t("installed schemes", () -> FileSystemProvider.installedProviders().stream().map(FileSystemProvider::getScheme).sorted().toList());
        t("Path.of fs identity", () -> Path.of(".").getFileSystem() == d);
        t("Paths fs identity", () -> Paths.get("x").getFileSystem() == d);
        t("File.toPath fs identity", () -> new File("x").toPath().getFileSystem() == d);
        t("provider identity", () -> Path.of("x").getFileSystem().provider() == d.provider());
        t("getFileSystem file uri", () -> FileSystems.getFileSystem(URI.create("file:///")) == d);
        t("getFileSystem bad scheme", () -> FileSystems.getFileSystem(URI.create("nope:///")));
        t("newFileSystem file uri", () -> FileSystems.newFileSystem(URI.create("file:///"), Map.of()));
        t("provider.getPath", () -> d.provider().getPath(URI.create("file:///tmp")).toString().replace('\\', '/'));
        t("Path.of uri", () -> Path.of(URI.create("file:///tmp/x")).toString().replace('\\', '/'));
        Path zip = Files.createTempFile("sbr2tail", ".zip");
        try (ZipOutputStream z = new ZipOutputStream(Files.newOutputStream(zip))) { z.putNextEntry(new ZipEntry("a.txt")); z.write("hi".getBytes()); z.closeEntry(); }
        t("zip fs", () -> { try (FileSystem zf = FileSystems.newFileSystem(zip)) { return zf.provider().getScheme() + "," + Files.readString(zf.getPath("a.txt")) + "," + zf.provider().getScheme() + "," + (zf.provider() != d.provider()); } });
        t("zip fs uri", () -> { try (FileSystem zf = FileSystems.newFileSystem(URI.create("jar:" + zip.toUri()), Map.of())) { return Files.exists(zf.getPath("a.txt")); } });
        t("zip fs loader", () -> { try (FileSystem zf = FileSystems.newFileSystem(zip, (ClassLoader) null)) { return zf.getRootDirectories().iterator().next().toString(); } });
        t("jrt fs", () -> FileSystems.getFileSystem(URI.create("jrt:/")).provider().getScheme());
        t("jrt path", () -> Files.exists(FileSystems.getFileSystem(URI.create("jrt:/")).getPath("modules", "java.base")));
        t("default is open", () -> d.isOpen() + "," + d.isReadOnly() + "," + d.getSeparator().length());
        t("attribute views", () -> d.supportedFileAttributeViews().contains("basic"));
        t("stores", () -> d.getFileStores().iterator().hasNext());
        t("user principal lookup", () -> d.getUserPrincipalLookupService() != null);
        t("watch service", () -> { try (WatchService w = d.newWatchService()) { return w != null; } });
        Files.deleteIfExists(zip);
    }
}
