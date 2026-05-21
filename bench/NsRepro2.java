import javax.xml.parsers.SAXParser;
import javax.xml.parsers.SAXParserFactory;
import org.xml.sax.Attributes;
import org.xml.sax.helpers.DefaultHandler;
import java.io.ByteArrayInputStream;

public class NsRepro2 {
    static void run(String label, String xml) throws Exception {
        System.out.println("--- " + label + " ---");
        SAXParserFactory f = SAXParserFactory.newInstance();
        f.setNamespaceAware(true);
        SAXParser p = f.newSAXParser();
        p.parse(new ByteArrayInputStream(xml.getBytes("UTF-8")), new DefaultHandler() {
            public void startPrefixMapping(String prefix, String uri) {
                System.out.println("  startPrefixMapping prefix='" + prefix + "' uri='" + uri + "'");
            }
            public void startElement(String uri, String localName, String qName, Attributes attrs) {
                System.out.println("  startElement uri='" + uri + "' local='" + localName + "' qName='" + qName + "'");
            }
        });
    }
    public static void main(String[] args) throws Exception {
        run("default ns", "<root xmlns=\"http://example.com/ns\"><child/></root>");
        run("prefixed ns", "<a:root xmlns:a=\"http://example.com/p\"><a:child/></a:root>");
        run("no ns", "<root><child/></root>");
    }
}
