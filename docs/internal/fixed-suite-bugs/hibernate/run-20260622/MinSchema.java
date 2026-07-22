import javax.xml.validation.SchemaFactory;
import javax.xml.transform.stream.StreamSource;
import java.net.URL;
import java.io.InputStream;
import static javax.xml.XMLConstants.W3C_XML_SCHEMA_NS_URI;

// Minimal, Hibernate-free repro of the LocalXsdResolver.resolveLocalXsdSchema
// hang: SchemaFactory(Xerces).newSchema() parsing a JPA ORM XSD.
public class MinSchema {
    public static void main(String[] a) throws Exception {
        String res = a.length > 0 ? a[0] : "org/hibernate/jpa/orm_1_0.xsd";
        System.out.println("@@LOCATE " + res); System.out.flush();
        URL url = MinSchema.class.getClassLoader().getResource(res);
        System.out.println("@@URL=" + url); System.out.flush();
        System.out.println("@@NEWINSTANCE"); System.out.flush();
        SchemaFactory sf = SchemaFactory.newInstance(W3C_XML_SCHEMA_NS_URI);
        System.out.println("@@SF=" + sf.getClass().getName()); System.out.flush();
        System.out.println("@@NEWSCHEMA"); System.out.flush();
        InputStream in = url.openStream();
        sf.newSchema(new StreamSource(in));
        System.out.println("@@SCHEMA_OK"); System.out.flush();
    }
}
