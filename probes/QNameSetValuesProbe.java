import com.sun.org.apache.xerces.internal.util.XMLAttributesImpl;
import com.sun.org.apache.xerces.internal.xni.QName;

// `XMLAttributesImpl.addAttributeNS` @pc=131 dispatches
// `QName.setValues(QName)` through the single-pass inline cache, and that ONE
// site is what the Hazelcast XSD failure bisects to (deny it and the failure is
// green; allow only it and the failure returns).
//
// `AddAttrNsProbe` drove the same method 20 000 times and matched HotSpot — but
// it only ever read back `getQName` (rawname) and `getValue`. `setValues(QName)`
// copies FOUR fields (prefix, localpart, rawname, uri), and a wrong `uri` is
// invisible to that probe while being exactly what breaks namespace binding in
// the validator. This one asserts all four, on both the raw QName call and
// through the real `addAttributeNS`.
//
// It also calls the SIBLING overload `setValues(String,String,String,String)`
// so both are compiled: if the two overloads collide anywhere in dispatch,
// the collision needs both present.
//
// Needs:
//   --add-exports java.xml/com.sun.org.apache.xerces.internal.util=ALL-UNNAMED
//   --add-exports java.xml/com.sun.org.apache.xerces.internal.xni=ALL-UNNAMED
public class QNameSetValuesProbe {

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        boolean alsoFourArg = args.length <= 1 || !args[1].equals("no4");

        int rawBad = 0;
        int attrBad = 0;
        String firstRaw = null;
        String firstAttr = null;

        QName dst = new QName();
        QName spare = new QName();
        for (int i = 0; i < iterations; i++) {
            String prefix = "p" + (i % 7);
            String local = "l" + (i % 11);
            String rawname = prefix + ":" + local;
            String uri = "urn:u" + (i % 5);

            QName src = new QName(prefix, local, rawname, uri);
            dst.setValues(src);
            if (!eq(dst.prefix, prefix) || !eq(dst.localpart, local)
                    || !eq(dst.rawname, rawname) || !eq(dst.uri, uri)) {
                rawBad++;
                if (firstRaw == null) {
                    firstRaw = "i=" + i + " got[" + dst.prefix + "," + dst.localpart + ","
                            + dst.rawname + "," + dst.uri + "] want[" + prefix + "," + local
                            + "," + rawname + "," + uri + "]";
                }
            }
            if (alsoFourArg) {
                spare.setValues(prefix, local, rawname, uri);
            }

            // The real caller: `addAttributeNS` copies the QName into the
            // attribute's own name via the site under investigation, then the
            // validator reads it back through getURI/getLocalName/getPrefix.
            XMLAttributesImpl attrs = new XMLAttributesImpl();
            for (int j = 0; j < 12; j++) {
                QName q = new QName("q" + j, "n" + j, "q" + j + ":n" + j, "urn:a" + (j % 3));
                attrs.addAttributeNS(q, "CDATA", "v" + j);
            }
            for (int j = 0; j < attrs.getLength(); j++) {
                String wantUri = "urn:a" + (j % 3);
                String wantLocal = "n" + j;
                String wantPrefix = "q" + j;
                String wantRaw = "q" + j + ":n" + j;
                if (!eq(attrs.getURI(j), wantUri) || !eq(attrs.getLocalName(j), wantLocal)
                        || !eq(attrs.getPrefix(j), wantPrefix) || !eq(attrs.getQName(j), wantRaw)) {
                    attrBad++;
                    if (firstAttr == null) {
                        firstAttr = "i=" + i + " j=" + j + " got[" + attrs.getPrefix(j) + ","
                                + attrs.getLocalName(j) + "," + attrs.getQName(j) + ","
                                + attrs.getURI(j) + "] want[" + wantPrefix + "," + wantLocal
                                + "," + wantRaw + "," + wantUri + "]";
                    }
                }
            }
        }

        System.out.println("QNAMESET: iterations=" + iterations + " fourArg=" + alsoFourArg
                + " rawBad=" + rawBad + " attrBad=" + attrBad);
        if (firstRaw != null) {
            System.out.println("QNAMESET-FIRST-RAW: " + firstRaw);
        }
        if (firstAttr != null) {
            System.out.println("QNAMESET-FIRST-ATTR: " + firstAttr);
        }
    }

    private static boolean eq(String a, String b) {
        return a == null ? b == null : a.equals(b);
    }
}
