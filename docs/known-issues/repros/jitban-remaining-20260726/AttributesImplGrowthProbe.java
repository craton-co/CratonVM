import org.xml.sax.helpers.AttributesImpl;

// HIB-LONGTAIL.2 repro: AttributesImpl.ensureCapacity is reported to pass a
// corrupted int count to anewarray during array growth (observed
// Object[1677721600] -- garbage huge size), during Hibernate's qualified
// table bootstrap. This is a pure-JDK class (org.xml.sax.helpers), no
// external jar needed. Repeatedly add attributes past the internal array's
// initial capacity (forcing many ensureCapacity growth calls across many
// independent AttributesImpl instances) and verify every attribute's
// name/value round-trips correctly with the right final count.
public class AttributesImplGrowthProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int attrsPerInstance = args.length > 1 ? Integer.parseInt(args[1]) : 40;
        int failures = 0;

        for (int i = 0; i < iterations; i++) {
            AttributesImpl attrs = new AttributesImpl();
            int n = (i % 5 == 0) ? attrsPerInstance * 3 : attrsPerInstance;
            for (int j = 0; j < n; j++) {
                attrs.addAttribute("", "local" + j, "qname" + i + "_" + j, "CDATA", "value" + i + "_" + j);
            }

            if (attrs.getLength() != n) {
                failures++;
                if (failures <= 5) {
                    System.out.println("LENGTH MISMATCH at i=" + i + " expected=" + n
                            + " got=" + attrs.getLength());
                }
            } else {
                for (int j = 0; j < n; j++) {
                    String expectedQName = "qname" + i + "_" + j;
                    String expectedValue = "value" + i + "_" + j;
                    if (!expectedQName.equals(attrs.getQName(j)) || !expectedValue.equals(attrs.getValue(j))) {
                        failures++;
                        if (failures <= 5) {
                            System.out.println("VALUE MISMATCH at i=" + i + " j=" + j
                                    + " qname=" + attrs.getQName(j) + " value=" + attrs.getValue(j));
                        }
                        break;
                    }
                }
            }

            // Removal also drives internal array compaction, another path
            // through the same growth/shrink bookkeeping.
            if (n > 2) {
                attrs.removeAttribute(n / 2);
                if (attrs.getLength() != n - 1) {
                    failures++;
                    if (failures <= 5) {
                        System.out.println("POST-REMOVE LENGTH MISMATCH at i=" + i);
                    }
                }
            }

            if (i % 2000 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
        }
        System.out.println("DONE iterations=" + iterations + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
