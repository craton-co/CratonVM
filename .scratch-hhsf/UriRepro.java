import java.net.URI;
import java.net.URISyntaxException;
public class UriRepro {
    static void test(String s) {
        try {
            URI u = new URI(s);
            System.out.println("ACCEPTED  [" + s + "] -> scheme=" + u.getScheme());
        } catch (URISyntaxException e) {
            System.out.println("REJECTED  [" + s + "] -> URISyntaxException: " + e.getReason());
        }
    }
    public static void main(String[] args) {
        test("not a valid uri :{}");
        test("https://example.com");
        test("foo bar");
        test(":{}");
        test("a b c");
        test("mailto:user@example.com");
        test("/relative/path");
        test("urn:isbn:0451450523");
    }
}
