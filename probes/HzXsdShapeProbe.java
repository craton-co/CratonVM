import java.io.InputStream;
import java.util.zip.ZipFile;

import javax.xml.parsers.DocumentBuilderFactory;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.NamedNodeMap;
import org.w3c.dom.Node;
import org.w3c.dom.NodeList;

import com.hazelcast.config.Config;

// The instance document parses IDENTICALLY to HotSpot in the warm process
// (HzWarmShapeProbe), so the damage is on the schema side. This hashes the
// XSD's own parse — the document that actually drives addAttributeNS hard,
// with thousands of elements and many fAttributes growth crossings — after
// running Config.load() first, so the measurement happens in the state the
// failure occupies rather than a cold one.
//
// Writes the full path dump (element paths with namespace URIs and every
// attribute's uri/localname/value) so CratonVM and HotSpot can be diffed line
// by line instead of compared by one hash that says only "different".
//
// Usage: HzXsdShapeProbe <jar> <entry> [outfile] [warmRounds]
public class HzXsdShapeProbe {

    private static final StringBuilder PATHS = new StringBuilder();
    private static int elements = 0;
    private static int maxDepth = 0;

    public static void main(String[] args) throws Exception {
        String jar = args[0];
        String entry = args[1];
        String out = args.length > 2 ? args[2] : null;
        int warm = args.length > 3 ? Integer.parseInt(args[3]) : 1;

        for (int i = 0; i < warm; i++) {
            try {
                Config.load();
                System.out.println("XSDSHAPE load=ok");
            }
            catch (Throwable t) {
                System.out.println("XSDSHAPE load=FAIL");
            }
        }

        Document doc;
        try (ZipFile zf = new ZipFile(jar);
                InputStream in = zf.getInputStream(zf.getEntry(entry))) {
            DocumentBuilderFactory f = DocumentBuilderFactory.newInstance();
            f.setNamespaceAware(true);
            doc = f.newDocumentBuilder().parse(in);
        }

        Element root = doc.getDocumentElement();
        walk(root, 0, "{" + root.getNamespaceURI() + "}" + root.getLocalName());
        System.out.println("XSDSHAPE entry=" + entry + " elements=" + elements
                + " maxDepth=" + maxDepth + " hash=" + PATHS.toString().hashCode()
                + " chars=" + PATHS.length());
        if (out != null) {
            java.nio.file.Files.write(java.nio.file.Paths.get(out),
                    PATHS.toString().getBytes("UTF-8"));
        }
    }

    private static void walk(Node n, int depth, String path) {
        if (depth > maxDepth) {
            maxDepth = depth;
        }
        NodeList kids = n.getChildNodes();
        for (int i = 0; i < kids.getLength(); i++) {
            Node c = kids.item(i);
            if (c.getNodeType() != Node.ELEMENT_NODE) {
                continue;
            }
            elements++;
            String p = path + "/{" + c.getNamespaceURI() + "}" + c.getLocalName();
            StringBuilder ab = new StringBuilder();
            NamedNodeMap at = c.getAttributes();
            for (int j = 0; at != null && j < at.getLength(); j++) {
                Node x = at.item(j);
                ab.append("@{").append(x.getNamespaceURI()).append("}")
                  .append(x.getLocalName()).append("=").append(x.getNodeValue()).append(";");
            }
            PATHS.append(p).append("[").append(ab).append("]\n");
            walk(c, depth + 1, p);
        }
    }
}
