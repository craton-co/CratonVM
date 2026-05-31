import java.io.File;
import java.net.URI;
public class FileUri {
    public static void main(String[] a) {
        File f = new File("/patha/pathb^/pathc");
        System.out.println("toURI="+f.toURI().toString());
        // also a few special chars
        System.out.println("space="+new File("/a b/c").toURI());
        System.out.println("hash="+new File("/a#b/c").toURI());
        // URI direct
        System.out.println("URI(jar:file:...^): "+java.net.URI.create("file:/patha/pathb%5E/pathc"));
    }
}
