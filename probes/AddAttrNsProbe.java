import com.sun.org.apache.xerces.internal.util.XMLAttributesImpl;
import com.sun.org.apache.xerces.internal.xni.QName;

// Exercises the REAL `XMLAttributesImpl.addAttributeNS` — the one method whose
// compilation makes Hazelcast's config schema validation fail (denying JIT on it
// alone turns the failure green). Not a replica: this drives the actual JDK
// class, so a divergence here localises the defect inside that method rather
// than in how it is dispatched to.
//
// The growth path is what matters: `addAttributeNS` does
// `if (fLength++ == fAttributes.length)` and then reallocates, so crossing the
// initial capacity repeatedly is the interesting shape.
//
// Needs:
//   --add-exports java.xml/com.sun.org.apache.xerces.internal.util=ALL-UNNAMED
//   --add-exports java.xml/com.sun.org.apache.xerces.internal.xni=ALL-UNNAMED
public class AddAttrNsProbe {

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int attrsPerRound = args.length > 1 ? Integer.parseInt(args[1]) : 12;

        long checksum = 0;
        int lengthMismatches = 0;
        int readbackMismatches = 0;

        for (int i = 0; i < iterations; i++) {
            XMLAttributesImpl attrs = new XMLAttributesImpl();
            for (int j = 0; j < attrsPerRound; j++) {
                String local = "a" + j;
                String uri = "urn:x" + (j % 3);
                QName q = new QName("p", local, "p:" + local, uri);
                attrs.addAttributeNS(q, "CDATA", "v" + j);
            }
            if (attrs.getLength() != attrsPerRound) {
                lengthMismatches++;
            }
            for (int j = 0; j < attrs.getLength(); j++) {
                String qn = attrs.getQName(j);
                String v = attrs.getValue(j);
                if (qn == null || v == null) {
                    readbackMismatches++;
                    continue;
                }
                // Position j must still hold the j-th attribute.
                if (!qn.equals("p:a" + j) || !v.equals("v" + j)) {
                    readbackMismatches++;
                }
                checksum = checksum * 31 + qn.hashCode();
                checksum = checksum * 31 + v.hashCode();
            }
            checksum = checksum * 31 + attrs.getLength();
        }

        System.out.println("ADDATTRNS: iterations=" + iterations
                + " attrsPerRound=" + attrsPerRound
                + " lengthMismatches=" + lengthMismatches
                + " readbackMismatches=" + readbackMismatches);
        System.out.println("ADDATTRNS-CHECKSUM=" + checksum);
    }
}
