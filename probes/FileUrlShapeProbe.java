// What shape does a File-derived URL have?
//
// Tomcat's `StandardRoot.processWebInfLib` hands `possibleJar.getURL()` to
// `createWebResourceSet`, which resolves it. On CratonVM that produced
//   Unable to create WebResourceSet from
//   [C:\craton\CratonVM\apps\tomcat\file:\C:\...\WEB-INF\lib\bug69135-lib.jar]
// — a `file:` URL that has been RESOLVED AGAINST THE DOC BASE, which only
// happens when it does not look absolute. Two candidate causes, and the diff
// against HotSpot separates them: the URL is built with backslashes (so its
// path is not a path), or it is missing the leading `/` after `file:`.
//
// Every line is `key=value` so a run diffs byte-for-byte against HotSpot.
import java.io.File;
import java.net.URI;
import java.net.URL;

public class FileUrlShapeProbe {
    static void show(String label, File f) throws Exception {
        URI uri = f.toURI();
        URL url = uri.toURL();
        System.out.println(label + ".path=" + f.getPath());
        System.out.println(label + ".absolute=" + f.isAbsolute());
        System.out.println(label + ".uri=" + uri);
        System.out.println(label + ".uri.scheme=" + uri.getScheme());
        System.out.println(label + ".uri.path=" + uri.getPath());
        System.out.println(label + ".uri.opaque=" + uri.isOpaque());
        System.out.println(label + ".url=" + url);
        System.out.println(label + ".url.protocol=" + url.getProtocol());
        System.out.println(label + ".url.path=" + url.getPath());
        System.out.println(label + ".url.file=" + url.getFile());
        System.out.println(label + ".url.host=" + String.valueOf(url.getHost()));
        // The exact operation Tomcat performs on it.
        System.out.println(label + ".roundtrip.uri=" + url.toURI());
        System.out.println(label + ".roundtrip.file=" + new File(url.toURI()).getPath());
    }

    public static void main(String[] args) throws Exception {
        // A relative path made absolute, which is what Tomcat's
        // `FileResource` holds, and the same file named absolutely.
        File rel = new File("probes/FileUrlShapeProbe.java");
        show("abs", rel.getAbsoluteFile());

        // A directory, because `toURI` appends a trailing slash for one and
        // that is part of the shape.
        show("dir", new File(".").getAbsoluteFile());

        System.out.println("PASS FileUrlShapeProbe");
    }
}
