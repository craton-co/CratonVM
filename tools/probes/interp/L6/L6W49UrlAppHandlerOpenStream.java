// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: a `java.net.URL` built with an
// APPLICATION `URLStreamHandler` (`new URL(null, spec, handler)`) is served by
// that handler's `openConnection(URL).getInputStream()`, whatever its scheme
// -- `jar:`, `file:`, `http:` included -- in `--compatible` as on HotSpot and
// under `--jdk-only`.
//
// This is the shape of Spring Boot's fat-jar URLs
// (`jar:nested:/app.jar/!BOOT-INF/classes/!/x/Y.class`, handler
// `org.springframework.boot.loader.net.protocol.jar.Handler`). Until wave 49
// `--compatible`'s `URL.openStream` native read the URL's TEXT and ignored its
// handler, opening the DIRECTORY entry `BOOT-INF/classes/` as a nested jar
// ("invalid Zip archive: Could not find EOCD"), so `cratonvm --compatible -jar`
// of a Spring Boot 4 fat jar died at its first component scan
// (`docs/internal/fixed-bugs/interpreter-L6-compatible-fat-jar-reads-boot-inf-classes-as-a-nested-jar-FIXED-20261013.md`).
//
// The handler here prefixes what it reads with `via-handler:`, so every row
// says who served the bytes. Positive controls (what `--compatible` printed
// before the fix, or with `CRATONVM_COMPAT_URL_APP_HANDLER=0`):
//   dir-openStream     IOException ... Could not find EOCD
//   lib-openStream     hello from inner      (the text reader, no prefix)
//   file-openStream    FileNotFoundException (the text reader read the disk)
//   http-openStream    IOException ... (the text reader dialed 127.0.0.1:1)
//   missing            the text reader's own IOException text
// The `dir-*` row runs first (cold VM) AND again last.
//
// HotSpot 25.0.3 prints (measured):
//     dir-openStream via-handler:hello from classes
//     dir-openConnection via-handler:hello from classes class=L6W49UrlAppHandlerOpenStream$Conn
//     lib-openStream via-handler:hello from inner
//     missing java.io.FileNotFoundException: missing: smoketest/Nope.txt
//     file-openStream via-handler:file /nonexistent-l6w49/abc.txt
//     http-openStream via-handler:http /x
//     archive-openStream via-handler:archive /a/b.txt
//     handler-identity true
//     dir-openStream-again via-handler:hello from classes
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.File;
import java.io.FileNotFoundException;
import java.io.FileOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.net.URL;
import java.net.URLConnection;
import java.net.URLStreamHandler;
import java.nio.charset.StandardCharsets;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;
import java.util.zip.ZipInputStream;
import java.util.zip.ZipOutputStream;

public class L6W49UrlAppHandlerOpenStream {
    static File jar;

    static final class Conn extends URLConnection {
        Conn(URL u) {
            super(u);
        }

        @Override
        public void connect() {}

        @Override
        public InputStream getInputStream() throws IOException {
            String protocol = url.getProtocol();
            String file = url.getFile();
            if (!protocol.equals("jar")) {
                return bytes("via-handler:" + protocol + " " + file);
            }
            // Spring Boot's grammar: nested:<outer>/!<entry>!/<resource>
            String rest = file.substring(file.indexOf("/!") + 2);
            int sep = rest.indexOf("!/");
            String entry = rest.substring(0, sep);
            String resource = rest.substring(sep + 2);
            try (ZipFile zf = new ZipFile(jar)) {
                if (entry.endsWith("/")) {
                    ZipEntry ze = zf.getEntry(entry + resource);
                    if (ze == null) {
                        throw new FileNotFoundException("missing: " + resource);
                    }
                    return bytes("via-handler:" + read(zf.getInputStream(ze)));
                }
                try (ZipInputStream in = new ZipInputStream(zf.getInputStream(zf.getEntry(entry)))) {
                    for (ZipEntry ze; (ze = in.getNextEntry()) != null; ) {
                        if (ze.getName().equals(resource)) {
                            return bytes("via-handler:" + read(in));
                        }
                    }
                }
                throw new FileNotFoundException("missing: " + resource);
            }
        }
    }

    static final class Handler extends URLStreamHandler {
        @Override
        protected URLConnection openConnection(URL u) {
            return new Conn(u);
        }
    }

    static final Handler HANDLER = new Handler();

    static InputStream bytes(String s) {
        return new ByteArrayInputStream(s.getBytes(StandardCharsets.UTF_8));
    }

    static String read(InputStream in) throws IOException {
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        in.transferTo(out);
        return out.toString(StandardCharsets.UTF_8);
    }

    static byte[] zip(String[][] entries) throws IOException {
        ByteArrayOutputStream bout = new ByteArrayOutputStream();
        try (ZipOutputStream zout = new ZipOutputStream(bout)) {
            for (String[] e : entries) {
                zout.putNextEntry(new ZipEntry(e[0]));
                if (e[1] != null) {
                    zout.write(e[1].getBytes(StandardCharsets.UTF_8));
                }
                zout.closeEntry();
            }
        }
        return bout.toByteArray();
    }

    static URL url(String spec) throws IOException {
        return new URL(null, spec, HANDLER);
    }

    static void row(String label, URL u) {
        try (InputStream in = u.openStream()) {
            System.out.println(label + " " + read(in));
        } catch (Throwable t) {
            System.out.println(label + " " + t);
        }
    }

    public static void main(String[] args) throws Exception {
        jar = File.createTempFile("l6w49-fat", ".jar");
        jar.deleteOnExit();
        byte[] inner = zip(new String[][] {{"META-INF/x.txt", "hello from inner"}});
        ByteArrayOutputStream bout = new ByteArrayOutputStream();
        try (ZipOutputStream zout = new ZipOutputStream(bout)) {
            zout.putNextEntry(new ZipEntry("BOOT-INF/"));
            zout.closeEntry();
            zout.putNextEntry(new ZipEntry("BOOT-INF/classes/"));
            zout.closeEntry();
            zout.putNextEntry(new ZipEntry("BOOT-INF/classes/smoketest/Hello.txt"));
            zout.write("hello from classes".getBytes(StandardCharsets.UTF_8));
            zout.closeEntry();
            zout.putNextEntry(new ZipEntry("BOOT-INF/lib/inner.jar"));
            zout.write(inner);
            zout.closeEntry();
        }
        try (FileOutputStream f = new FileOutputStream(jar)) {
            f.write(bout.toByteArray());
        }
        String outer = jar.toURI().getPath();

        URL dir = url("jar:nested:" + outer + "/!BOOT-INF/classes/!/smoketest/Hello.txt");
        row("dir-openStream", dir);
        try {
            URLConnection c = dir.openConnection();
            System.out.println("dir-openConnection " + read(c.getInputStream())
                    + " class=" + c.getClass().getName());
        } catch (Throwable t) {
            System.out.println("dir-openConnection " + t);
        }
        row("lib-openStream", url("jar:nested:" + outer + "/!BOOT-INF/lib/inner.jar!/META-INF/x.txt"));
        row("missing", url("jar:nested:" + outer + "/!BOOT-INF/classes/!/smoketest/Nope.txt"));
        row("file-openStream", url("file:/nonexistent-l6w49/abc.txt"));
        row("http-openStream", url("http://127.0.0.1:1/x"));
        row("archive-openStream", url("archive:/a/b.txt"));
        URL again = url("jar:nested:" + outer + "/!BOOT-INF/classes/!/smoketest/Hello.txt");
        System.out.println("handler-identity " + (again.openConnection() instanceof Conn));
        row("dir-openStream-again", again);
    }
}
