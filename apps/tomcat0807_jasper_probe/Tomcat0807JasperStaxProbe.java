import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;
import javax.xml.stream.XMLInputFactory;
import javax.xml.stream.XMLStreamReader;

public class Tomcat0807JasperStaxProbe {
    private static String scheme(byte[] bytes) throws Exception {
        XMLStreamReader reader =
                XMLInputFactory.newInstance().createXMLStreamReader(new ByteArrayInputStream(bytes));
        return reader.getCharacterEncodingScheme();
    }

    private static byte[] utf16be(String text) {
        return text.getBytes(StandardCharsets.UTF_16BE);
    }

    private static byte[] utf8Bom(String text) {
        byte[] body = text.getBytes(StandardCharsets.UTF_8);
        byte[] out = new byte[body.length + 3];
        out[0] = (byte) 0xEF;
        out[1] = (byte) 0xBB;
        out[2] = (byte) 0xBF;
        System.arraycopy(body, 0, out, 3, body.length);
        return out;
    }

    private static void expect(String label, String expected, String actual) {
        if (!expected.equals(actual)) {
            throw new AssertionError(label + " expected " + expected + " got " + actual);
        }
        System.out.println(label + "=" + actual);
    }

    public static void main(String[] args) throws Exception {
        expect(
                "utf16be",
                "UTF-16BE",
                scheme(utf16be("<?xml version=\"1.0\" encoding=\"UTF-16BE\"?><root/>")));
        expect(
                "utf8bom",
                "UTF-8",
                scheme(utf8Bom("<?xml version=\"1.0\" encoding=\"UTF-8\"?><root/>")));
        System.out.println("OK");
    }
}
