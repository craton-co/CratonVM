import java.io.InputStream;
import java.util.zip.ZipEntry;
import java.util.zip.ZipFile;

import javax.xml.parsers.DocumentBuilder;
import javax.xml.parsers.DocumentBuilderFactory;

import org.w3c.dom.Document;
import org.w3c.dom.Element;
import org.w3c.dom.Node;
import org.w3c.dom.NodeList;

// Hazelcast validates its config by handing the PARSED DOM to
// `Validator.validate(new DOMSource(doc))` (AbstractXmlConfigHelper.schemaValidation).
// When that reports `kubernetes` where a top-level <hazelcast> child is
// expected, the schema is not the problem — the DOM's shape is: <kubernetes>
// lives under <network><join>, so the validator only sees it at top level if
// the tree it walked was flattened.
//
// This prints the element tree as depth-indented paths, so CratonVM's DOM can
// be diffed against HotSpot's for the same bytes. Usage:
//   java XmlDomShapeProbe <hazelcast.jar> [entry]
public class XmlDomShapeProbe {

    private static int elementCount = 0;
    private static int maxDepth = 0;
    private static final StringBuilder PATHS = new StringBuilder();

    public static void main(String[] args) throws Exception {
        String jar = args.length > 0 ? args[0] : null;
        String entry = args.length > 1 ? args[1] : "hazelcast-default.xml";
        if (jar == null) {
            System.out.println("usage: XmlDomShapeProbe <jar> [entry]");
            return;
        }

        Document doc;
        try (ZipFile zf = new ZipFile(jar)) {
            ZipEntry ze = zf.getEntry(entry);
            if (ze == null) {
                System.out.println("entry not found: " + entry);
                return;
            }
            try (InputStream in = zf.getInputStream(ze)) {
                DocumentBuilderFactory dbf = DocumentBuilderFactory.newInstance();
                dbf.setNamespaceAware(true);
                DocumentBuilder db = dbf.newDocumentBuilder();
                doc = db.parse(in);
            }
        }

        Element root = doc.getDocumentElement();
        System.out.println("root = " + root.getNodeName() + "  ns=" + root.getNamespaceURI());
        walk(root, 0, root.getNodeName());

        System.out.println("elements=" + elementCount + " maxDepth=" + maxDepth);

        // The two facts the validator actually depends on.
        System.out.println("kubernetesParentPath = " + findParentPath(root, "kubernetes", root.getNodeName()));
        System.out.println("topLevelChildren = " + directChildNames(root));

        System.out.println("PATHS-HASH = " + PATHS.toString().hashCode());
    }

    private static void walk(Node n, int depth, String path) {
        if (depth > maxDepth) {
            maxDepth = depth;
        }
        NodeList kids = n.getChildNodes();
        for (int i = 0; i < kids.getLength(); i++) {
            Node k = kids.item(i);
            if (k.getNodeType() != Node.ELEMENT_NODE) {
                continue;
            }
            elementCount++;
            String p = path + "/" + k.getNodeName();
            PATHS.append(p).append('\n');
            walk(k, depth + 1, p);
        }
    }

    /** Path of the first element named `name`, or "NOT-FOUND". */
    private static String findParentPath(Node n, String name, String path) {
        NodeList kids = n.getChildNodes();
        for (int i = 0; i < kids.getLength(); i++) {
            Node k = kids.item(i);
            if (k.getNodeType() != Node.ELEMENT_NODE) {
                continue;
            }
            String p = path + "/" + k.getNodeName();
            if (k.getNodeName().equals(name) || k.getLocalName() != null && k.getLocalName().equals(name)) {
                return p;
            }
            String found = findParentPath(k, name, p);
            if (!found.equals("NOT-FOUND")) {
                return found;
            }
        }
        return "NOT-FOUND";
    }

    private static String directChildNames(Node n) {
        StringBuilder sb = new StringBuilder();
        NodeList kids = n.getChildNodes();
        for (int i = 0; i < kids.getLength(); i++) {
            Node k = kids.item(i);
            if (k.getNodeType() != Node.ELEMENT_NODE) {
                continue;
            }
            if (sb.length() > 0) {
                sb.append(',');
            }
            sb.append(k.getNodeName());
        }
        return sb.toString();
    }
}
