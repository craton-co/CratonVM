import jakarta.xml.bind.JAXBContext;
import jakarta.xml.bind.JAXBElement;
import jakarta.xml.bind.Marshaller;
import jakarta.xml.bind.Unmarshaller;
import jakarta.xml.bind.annotation.*;

import javax.xml.namespace.QName;
import java.io.StringReader;
import java.io.StringWriter;
import java.util.ArrayList;
import java.util.List;

public class JaxbQNameProbe {

    @XmlRootElement(name = "widget")
    @XmlAccessorType(XmlAccessType.FIELD)
    public static class Widget {
        @XmlAttribute
        public String id;

        // QName-typed field: exercises JAXB's QName runtime binding graph,
        // matching the "QName cannot be cast to QName" corruption shape.
        @XmlElement(name = "type")
        public QName type;

        @XmlElement(name = "refs")
        public List<QName> refs = new ArrayList<>();

        public Widget() {
        }

        public Widget(String id, QName type, List<QName> refs) {
            this.id = id;
            this.type = type;
            this.refs = refs;
        }
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        JAXBContext ctx = JAXBContext.newInstance(Widget.class);

        int failures = 0;
        for (int i = 0; i < iterations; i++) {
            QName type = new QName("type" + (i % 11) + "_" + i);
            List<QName> refs = new ArrayList<>();
            for (int j = 0; j < 3; j++) {
                refs.add(new QName("ref" + i + "-" + j));
            }
            Widget w = new Widget("w" + i, type, refs);

            Marshaller m = ctx.createMarshaller();
            StringWriter sw = new StringWriter();
            m.marshal(w, sw);
            String xml = sw.toString();

            Unmarshaller u = ctx.createUnmarshaller();
            Widget back;
            try {
                back = (Widget) u.unmarshal(new StringReader(xml));
            } catch (Exception e) {
                System.out.println("UNMARSHAL EXCEPTION at i=" + i + " xml=" + xml + " msg=" + e.getMessage());
                throw e;
            }

            if (back.type == null || !back.type.getLocalPart().equals(type.getLocalPart())
                    || !back.type.getNamespaceURI().equals(type.getNamespaceURI())) {
                failures++;
                if (failures <= 5) {
                    System.out.println("MISMATCH at i=" + i + " expected=" + type + " got=" + back.type
                            + "\nxml=" + xml);
                }
            }
            if (back.refs == null || back.refs.size() != refs.size()) {
                failures++;
                if (failures <= 5) {
                    System.out.println("REF-COUNT MISMATCH at i=" + i + " expected=" + refs.size()
                            + " got=" + (back.refs == null ? -1 : back.refs.size()));
                }
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
