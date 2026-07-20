import javax.xml.parsers.SAXParserFactory;
import javax.xml.parsers.SAXParser;
import org.xml.sax.InputSource;
import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;

/**
 * Companion to SaxManySmallFiles.java. Compares UTF-8 vs. US-ASCII encoded
 * parses of the same logical document to test whether Xerces's `UTF8Reader`
 * (com.sun.org.apache.xerces.internal.impl.io.UTF8Reader — NOT covered by
 * any existing native fast path, unlike scanQName/scanContent/etc.) is the
 * hot method behind the SaxManySmallFiles slowdown.
 *
 * RESULT (2026-07-20, dev tip, CratonVM jit=on): ASCII was NOT faster than
 * UTF-8 (in fact slightly slower: ~13.5ms vs ~8.7ms/parse) — this REFUTES
 * the UTF8Reader-decoder hypothesis. The bottleneck is in scanning/attribute/
 * namespace/grammar machinery shared by both encodings, not encoding-specific
 * byte decoding. Do not waste time on a UTF8Reader native fast path without
 * new evidence pointing back at it.
 */
public class SaxEncodingCompare {
    static final String BODY =
          "<taglib xmlns=\"http://jakarta.ee/xml/ns/jakartaee\" version=\"3.0\">\n"
        + "  <description>Sample tag library</description>\n"
        + "  <tlib-version>1.0</tlib-version>\n"
        + "  <short-name>sample</short-name>\n"
        + "  <uri>http://example.com/sample</uri>\n"
        + "  <tag>\n"
        + "    <name>hello</name>\n"
        + "    <tag-class>com.example.HelloTag</tag-class>\n"
        + "    <body-content>empty</body-content>\n"
        + "    <attribute>\n"
        + "      <name>value</name>\n"
        + "      <required>false</required>\n"
        + "      <rtexprvalue>true</rtexprvalue>\n"
        + "    </attribute>\n"
        + "  </tag>\n"
        + "</taglib>\n";

    static long timeParse(SAXParser parser, byte[] bytes, int reps) throws Exception {
        long start = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            parser.getXMLReader().parse(new InputSource(new ByteArrayInputStream(bytes)));
        }
        return (System.nanoTime() - start) / 1_000_000;
    }

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        SAXParserFactory factory = SAXParserFactory.newInstance();
        factory.setNamespaceAware(true);
        SAXParser parser = factory.newSAXParser();

        byte[] utf8Bytes = ("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n" + BODY).getBytes(StandardCharsets.UTF_8);
        byte[] asciiBytes = ("<?xml version=\"1.0\" encoding=\"US-ASCII\"?>\n" + BODY).getBytes(StandardCharsets.US_ASCII);

        long utf8Ms = timeParse(parser, utf8Bytes, reps);
        long asciiMs = timeParse(parser, asciiBytes, reps);

        System.out.println("UTF-8  reps=" + reps + " elapsedMs=" + utf8Ms + " avgUs=" + (utf8Ms * 1000L / reps));
        System.out.println("ASCII  reps=" + reps + " elapsedMs=" + asciiMs + " avgUs=" + (asciiMs * 1000L / reps));
        System.out.println("RESULT=OK");
    }
}
