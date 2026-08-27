import java.net.URI;
import java.nio.file.Path;
public class UriRawPathProbe {
    public static void main(String[] a) throws Exception {
        Path p = Path.of("/tmp/junit-123/te st.jar");
        URI u = p.toUri();
        System.out.println("Path.toUri()            = " + u);
        System.out.println("  .getRawPath()         = " + u.getRawPath());
        System.out.println("  .getPath()            = " + u.getPath());
        System.out.println("  .toASCIIString()      = " + u.toASCIIString());

        URI m = new URI("file:///tmp/junit-123/te%20st.jar");
        System.out.println("manual URI              = " + m);
        System.out.println("  .getRawPath()         = " + m.getRawPath());
        System.out.println("  .getPath()            = " + m.getPath());

        // The exact expression NestedPath.toUri() evaluates.
        System.out.println("nested: + rawPath       = " + ("nested:" + u.getRawPath()));
    }
}
