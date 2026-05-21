import javax.xml.parsers.SAXParser;
import javax.xml.parsers.SAXParserFactory;
import org.xml.sax.Attributes;
import org.xml.sax.helpers.DefaultHandler;
import java.io.ByteArrayInputStream;

public class NsRepro {
    public static void main(String[] args) throws Exception {
        String xml = "<root xmlns=\"http://example.com/ns\"><child/></root>";
        SAXParserFactory f = SAXParserFactory.newInstance();
        f.setNamespaceAware(true);
        SAXParser p = f.newSAXParser();
        p.parse(new ByteArrayInputStream(xml.getBytes("UTF-8")), new DefaultHandler() {
            public void startElement(String uri, String localName, String qName, Attributes attrs) {
                System.out.println("startElement uri='" + uri + "' local='" + localName + "' qName='" + qName + "'");
                for (int i = 0; i < attrs.getLength(); i++) {
                    System.out.println("  attr uri='" + attrs.getURI(i) + "' local='" + attrs.getLocalName(i)
                        + "' qName='" + attrs.getQName(i) + "' value='" + attrs.getValue(i) + "'");
                }
            }
        });
        System.out.println("DONE");
    }
}
