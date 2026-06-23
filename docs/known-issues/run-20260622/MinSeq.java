import javax.xml.validation.SchemaFactory;
import javax.xml.transform.stream.StreamSource;
import java.net.URL;
import java.io.InputStream;
import static javax.xml.XMLConstants.W3C_XML_SCHEMA_NS_URI;
// Replicate MappingXsdSupport.<clinit>: parse each XSD via a fresh SchemaFactory, in order.
public class MinSeq {
    static final String[] R = {
        "org/hibernate/xsd/mapping/mapping-3.1.0.xsd","org/hibernate/xsd/mapping/mapping-7.0.xsd",
        "org/hibernate/xsd/mapping/mapping-8.0.xsd","org/hibernate/jpa/orm_1_0.xsd",
        "org/hibernate/jpa/orm_2_0.xsd","org/hibernate/jpa/orm_2_1.xsd","org/hibernate/jpa/orm_2_2.xsd",
        "org/hibernate/jpa/orm_3_0.xsd","org/hibernate/jpa/orm_3_1.xsd","org/hibernate/jpa/orm_3_2.xsd",
        "org/hibernate/jpa/orm_4_0.xsd","org/hibernate/xsd/mapping/legacy-mapping-4.0.xsd",
        "org/hibernate/hibernate-mapping-4.0.xsd",
    };
    public static void main(String[] a) throws Exception {
        for (int i=0;i<R.length;i++){
            System.out.println("@@PARSE["+i+"] "+R[i]); System.out.flush();
            URL url = MinSeq.class.getClassLoader().getResource(R[i]);
            InputStream in = url.openStream();
            SchemaFactory.newInstance(W3C_XML_SCHEMA_NS_URI).newSchema(new StreamSource(in));
            in.close();
            System.out.println("@@OK["+i+"]"); System.out.flush();
        }
        System.out.println("@@ALLDONE"); System.out.flush();
    }
}
