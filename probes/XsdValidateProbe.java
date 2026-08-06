import java.io.ByteArrayInputStream;
import java.io.InputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

import javax.xml.XMLConstants;
import javax.xml.parsers.DocumentBuilder;
import javax.xml.parsers.DocumentBuilderFactory;
import javax.xml.transform.dom.DOMSource;
import javax.xml.transform.stream.StreamSource;
import javax.xml.validation.Schema;
import javax.xml.validation.SchemaFactory;
import javax.xml.validation.Validator;

import org.w3c.dom.Document;

// Standalone reproducer for the Hazelcast config schema-validation failure,
// with no Spring and no Hazelcast code: it does exactly what
// `AbstractXmlConfigHelper.schemaValidation` does — parse the config to a DOM,
// build a Schema from the bundled XSD, and `validate(new DOMSource(doc))`.
//
// The DOM itself is known-good (XmlDomShapeProbe reports a byte-identical tree
// shape on CratonVM and HotSpot), so a failure here is in the validator.
//
// Usage: java XsdValidateProbe <hazelcast.jar> [iterations]
// Prints `VALIDATE: ok=<n> fail=<n>` plus the first failure message.
public class XsdValidateProbe {

    public static void main(String[] args) throws Exception {
        String jar = args[0];
        int iterations = args.length > 1 ? Integer.parseInt(args[1]) : 1;

        byte[] xml;
        byte[] xsd;
        try (ZipFile zf = new ZipFile(jar)) {
            xml = read(zf, "hazelcast-default.xml");
            xsd = read(zf, "hazelcast-config-5.5.xsd");
        }
        System.out.println("xml=" + xml.length + " bytes  xsd=" + xsd.length + " bytes");

        int ok = 0;
        int fail = 0;
        String firstFailure = null;
        for (int i = 0; i < iterations; i++) {
            try {
                validateOnce(xml, xsd);
                ok++;
            }
            catch (Throwable t) {
                fail++;
                if (firstFailure == null) {
                    firstFailure = t.getClass().getName() + ": " + t.getMessage();
                    System.out.println("first failure at iteration " + i + ": " + firstFailure);
                }
            }
        }
        System.out.println("VALIDATE: ok=" + ok + " fail=" + fail);
    }

    private static void validateOnce(byte[] xml, byte[] xsd) throws Exception {
        DocumentBuilderFactory dbf = DocumentBuilderFactory.newInstance();
        dbf.setNamespaceAware(true);
        DocumentBuilder db = dbf.newDocumentBuilder();
        Document doc = db.parse(new ByteArrayInputStream(xml));

        SchemaFactory factory = SchemaFactory.newInstance(XMLConstants.W3C_XML_SCHEMA_NS_URI);
        Schema schema = factory.newSchema(new StreamSource(new ByteArrayInputStream(xsd)));
        Validator validator = schema.newValidator();
        validator.validate(new DOMSource(doc));
    }

    private static byte[] read(ZipFile zf, String name) throws Exception {
        ZipEntry ze = zf.getEntry(name);
        if (ze == null) {
            throw new IllegalStateException("entry not found: " + name);
        }
        try (InputStream in = zf.getInputStream(ze)) {
            return in.readAllBytes();
        }
    }
}
