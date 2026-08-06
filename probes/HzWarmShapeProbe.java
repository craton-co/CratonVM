import java.io.InputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

import javax.xml.parsers.DocumentBuilder;
import javax.xml.parsers.DocumentBuilderFactory;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.Node;
import org.w3c.dom.NodeList;

import com.hazelcast.config.Config;

// `XmlDomShapeProbe` shows CratonVM's DOM matching HotSpot's exactly — but it
// runs COLD, in a process that never did anything else, so the methods the
// failure needs compiled are still interpreted when it measures. That is the
// synthetic-replica trap: it proves the parser is right in a state the failure
// never occupies.
//
// This one measures the same shape in the WARM process: `Config.load()` first
// (which is the failure, and which compiles the whole scanner/validator graph),
// then the DOM hash, repeated. If the hash drifts after the first round, the
// damage is in the parse; if it stays put while `Config.load()` keeps failing,
// the parse is intact and the schema side is where to look.
//
// Paths carry the namespace URI, so a namespace-binding fault — the shape a
// corrupted attribute QName would produce — cannot hide behind a matching
// element name.
//
// Usage: HzWarmShapeProbe <hazelcast.jar> [rounds]
public class HzWarmShapeProbe {

    public static void main(String[] args) throws Exception {
        String jar = args[0];
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 5;

        for (int r = 0; r < rounds; r++) {
            String load;
            try {
                Config c = Config.load();
                load = "ok(" + c.getClusterName() + ")";
            }
            catch (Throwable t) {
                String m = t.getMessage();
                int q = m == null ? -1 : m.indexOf("element '");
                load = "FAIL" + (q >= 0 ? m.substring(q, Math.min(m.length(), q + 60)) : "");
            }
            Shape s = shape(jar, "hazelcast-default.xml");
            System.out.println("WARMSHAPE round=" + r + " load=" + load
                    + " elements=" + s.elements + " maxDepth=" + s.maxDepth
                    + " hash=" + s.hash + " kubernetes=" + s.kubernetesPath);
        }
    }

    private static final class Shape {
        int elements;
        int maxDepth;
        int hash;
        String kubernetesPath = "NOT-FOUND";
        final StringBuilder paths = new StringBuilder();
    }

    private static Shape shape(String jar, String entry) throws Exception {
        Document doc;
        try (ZipFile zf = new ZipFile(jar)) {
            ZipEntry ze = zf.getEntry(entry);
            try (InputStream in = zf.getInputStream(ze)) {
                DocumentBuilderFactory dbf = DocumentBuilderFactory.newInstance();
                dbf.setNamespaceAware(true);
                DocumentBuilder db = dbf.newDocumentBuilder();
                doc = db.parse(in);
            }
        }
        Shape s = new Shape();
        Element root = doc.getDocumentElement();
        walk(s, root, 0, "{" + root.getNamespaceURI() + "}" + root.getLocalName());
        s.hash = s.paths.toString().hashCode();
        return s;
    }

    private static void walk(Shape s, Node n, int depth, String path) {
        if (depth > s.maxDepth) {
            s.maxDepth = depth;
        }
        NodeList kids = n.getChildNodes();
        for (int i = 0; i < kids.getLength(); i++) {
            Node k = kids.item(i);
            if (k.getNodeType() != Node.ELEMENT_NODE) {
                continue;
            }
            s.elements++;
            String p = path + "/{" + k.getNamespaceURI() + "}" + k.getLocalName();
            // Attributes are part of the shape: a wrong attribute URI is what a
            // corrupted `XMLAttributesImpl` QName would look like, and it is
            // invisible in an element-only path dump.
            org.w3c.dom.NamedNodeMap at = k.getAttributes();
            if (at != null && at.getLength() > 0) {
                StringBuilder ab = new StringBuilder();
                for (int j = 0; j < at.getLength(); j++) {
                    Node a = at.item(j);
                    ab.append('@').append('{').append(a.getNamespaceURI()).append('}')
                      .append(a.getLocalName()).append('=').append(a.getNodeValue()).append(';');
                }
                p = p + "[" + ab + "]";
            }
            s.paths.append(p).append('\n');
            if (s.kubernetesPath.equals("NOT-FOUND") && "kubernetes".equals(k.getLocalName())) {
                s.kubernetesPath = p;
            }
            walk(s, k, depth + 1, path + "/{" + k.getNamespaceURI() + "}" + k.getLocalName());
        }
    }
}
