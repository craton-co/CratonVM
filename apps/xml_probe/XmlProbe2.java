import javax.xml.stream.*;
import java.io.*;
public class XmlProbe2 {
    public static void main(String[] a) throws Exception {
        XMLInputFactory f = XMLInputFactory.newInstance();
        XMLStreamReader r = f.createXMLStreamReader(new FileInputStream("/tmp/test.xml"));
        while (r.hasNext()) {
            int e = r.next();
            String n = "?";
            try { n = r.getLocalName(); } catch (Exception ex) {}
            String t = "?";
            try { t = r.getText(); } catch (Exception ex) {}
            System.out.println("event=" + e + " name=" + n + " text=[" + t + "]");
        }
        r.close();
    }
}
