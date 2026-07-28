import net.minidev.json.JSONValue;
import net.minidev.json.parser.JSONParser;
import java.util.Map;

// Minimal form of the json-smart round-trip residual: parse ONE document in a
// loop and report any iteration whose resulting key order differs from the
// first iteration's. The full probe always trips on this document, and always
// as a single one-off event, so this narrows the repro to a single parse call.
public class MiniJsonProbe {
    static final String DOC = "{\"empty_str\":\"\",\"empty_arr\":[],\"empty_obj\":{}}";

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 5000;
        JSONParser parser = new JSONParser(JSONParser.MODE_JSON_SIMPLE);
        String reference = null;
        int flips = 0;
        for (int i = 0; i < iterations; i++) {
            Object parsed = parser.parse(DOC);
            StringBuilder sb = new StringBuilder();
            for (Object k : ((Map<?, ?>) parsed).keySet()) sb.append(k).append(' ');
            String order = sb.toString();
            if (reference == null) {
                reference = order;
                System.out.println("reference order=" + order);
            } else if (!reference.equals(order)) {
                flips++;
                if (flips <= 10) System.out.println("ORDER FLIP at iter=" + i + " got=" + order
                        + " size=" + ((Map<?, ?>) parsed).size());
            }
        }
        System.out.println("DONE flips=" + flips);
        System.out.println(flips == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
