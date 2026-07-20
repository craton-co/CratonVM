import javax.xml.parsers.SAXParserFactory;
import javax.xml.parsers.SAXParser;
import org.xml.sax.InputSource;
import java.io.ByteArrayInputStream;
import java.nio.charset.StandardCharsets;

/**
 * Standalone, pure-JDK repro (no app classpath needed) for the TLD/JAR-scan
 * slowdown residual documented in
 * docs/known-issues/springboot/jetty-webserver-factory-poststartup-timeout-and-reflective-supertype-residuals.md.
 *
 * Isolates plain SAX parsing of a small (~500 byte) TLD-shaped XML document,
 * repeated with a REUSED SAXParser (matching how Tomcat's TldParser pools
 * Digester instances, so parser-construction cost doesn't confound the
 * measurement) — no file I/O, no classpath/jar scanning involved at all.
 *
 * HotSpot: ~40-100us/parse. CratonVM (dev tip 2026-07-20, jit=on): ~8-13ms/parse
 * — roughly 100-200x slower, entirely inside the parse() call. This is the
 * SAME order-of-magnitude slowdown seen in the real JettyServletWebServerFactoryTests
 * spikes (~185-194s cycles vs. HotSpot's 22.6s for the whole ~115-test class),
 * confirming the bottleneck is Xerces SAX-parsing execution cost, not any
 * file/jar/classloader I/O layer (those were separately ruled out — see the
 * known-issues doc's residual section for the JAR-open/ClassPath::new/
 * find_resource timing measurements that ruled them out).
 *
 * Run pattern (replace $CV with a built binary, $JDK with the JDK 25 home):
 *   javac SaxManySmallFiles.java
 *   "$JDK/bin/java" -cp . SaxManySmallFiles 3000      # HotSpot baseline
 *   $CV -cp . SaxManySmallFiles 300                    # CratonVM (slow)
 *   $CV --nojit -cp . SaxManySmallFiles 300             # JIT helps ~30%, doesn't close the gap
 */
public class SaxManySmallFiles {
    static final String XML = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
        + "<taglib xmlns=\"http://jakarta.ee/xml/ns/jakartaee\" version=\"3.0\">\n"
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

    public static void main(String[] args) throws Exception {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        byte[] bytes = XML.getBytes(StandardCharsets.UTF_8);
        SAXParserFactory factory = SAXParserFactory.newInstance();
        factory.setNamespaceAware(true);

        // Reused parser, like Tomcat's pooled Digester — isolates parse()
        // cost from newSAXParser() construction cost.
        SAXParser reusable = factory.newSAXParser();
        long start1 = System.nanoTime();
        for (int i = 0; i < reps; i++) {
            reusable.getXMLReader().parse(new InputSource(new ByteArrayInputStream(bytes)));
        }
        long elapsed1Ms = (System.nanoTime() - start1) / 1_000_000;
        System.out.println("REUSED-PARSER reps=" + reps + " elapsedMs=" + elapsed1Ms
            + " avgUsPerParse=" + ((elapsed1Ms * 1000L) / reps));

        // Fresh newSAXParser() cost alone (also slow under CratonVM, ~35x,
        // but a smaller relative contributor if the caller pools parsers).
        long start2 = System.nanoTime();
        int constructReps = Math.min(reps, 50);
        for (int i = 0; i < constructReps; i++) {
            factory.newSAXParser();
        }
        long elapsed2Ms = (System.nanoTime() - start2) / 1_000_000;
        System.out.println("NEW-PARSER-ONLY reps=" + constructReps + " elapsedMs=" + elapsed2Ms
            + " avgUsPerNewParser=" + ((elapsed2Ms * 1000L) / constructReps));

        System.out.println("RESULT=OK");
    }
}
