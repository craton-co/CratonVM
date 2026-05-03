import javax.xml.stream.*;
import javax.xml.stream.events.*;
import java.io.*;
public class XmlProbe {
    public static void main(String[] a) throws Exception {
        XMLInputFactory f = XMLInputFactory.newInstance();
        XMLStreamReader r = f.createXMLStreamReader(new FileInputStream("/tmp/test.xml"));
        int events = 0;
        int servers = 0;
        String firstName = null;
        StringBuilder cdata = new StringBuilder();
        while (r.hasNext()) {
            int e = r.next();
            events++;
            if (e == XMLStreamReader.START_ELEMENT) {
                String n = r.getLocalName();
                if (n.equals("server")) {
                    servers++;
                    if (firstName == null) firstName = r.getAttributeValue(null, "name");
                }
            }
            if (e == XMLStreamReader.CHARACTERS || e == XMLStreamReader.CDATA) {
                cdata.append(r.getText());
            }
        }
        r.close();
        System.out.println("events=" + events);
        System.out.println("servers=" + servers);
        System.out.println("firstName=" + firstName);
        System.out.println("cdata.contains.bracketed=" + cdata.toString().contains("<bracketed>"));
        System.out.println("OK");
    }
}
