import org.xml.sax.helpers.AttributesImpl;

/**
 * Sharper form of AttributesImplGrowthProbe: after every single addAttribute
 * it re-reads getLength(). If the STORED `length` field is being corrupted we
 * see a LEN-CORRUPT line before the growth loop ever blows up; if we only ever
 * see the OOM, the corruption is confined to the read of `length` inside
 * ensureCapacity.
 */
public class AttrsCorruptProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        int attrs = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        for (int i = 0; i < iterations; i++) {
            AttributesImpl a = new AttributesImpl();
            for (int j = 0; j < attrs; j++) {
                a.addAttribute("uri", "local" + j, "q" + i + "_" + j, "CDATA", "v" + i + "_" + j);
                int len = a.getLength();
                if (len != j + 1) {
                    System.out.println("LEN-CORRUPT i=" + i + " j=" + j + " len=" + len);
                    System.exit(1);
                }
            }
        }
        System.out.println("DONE iterations=" + iterations);
    }
}
