import net.minidev.json.JSONValue;
import net.minidev.json.JSONObject;
import net.minidev.json.JSONArray;
import net.minidev.json.parser.JSONParser;

public class JsonSmartProbe {
    // Mirrors JsonPathResultMatchersTests's shape: nested objects/arrays,
    // strings with escapes, numbers, mixed types -- enough variety to
    // exercise JSONParserString.read()/readS() and JSONParserBase.skipSpace()
    // (the exact methods named in the ban comment) across many cursor
    // positions and character classes.
    static final String[] DOCS = {
        "{\"a\":1,\"b\":2.5,\"c\":\"hello\",\"d\":true,\"e\":null,\"f\":[1,2,3]}",
        "{\"nested\":{\"x\":{\"y\":{\"z\":[1,2,3,4,5,6,7,8,9,10]}}}}",
        "[\"one\",\"two\",\"three\",{\"k\":\"v with \\\"escaped\\\" quotes\"}]",
        "{\"unicode\":\"caf\\u00e9 \\u4e2d\\u6587\", \"tab\":\"a\\tb\\nc\"}",
        "{\"arr\":[{\"id\":1,\"name\":\"alpha\"},{\"id\":2,\"name\":\"beta\"},{\"id\":3,\"name\":\"gamma\"}],\"count\":3,\"ok\":true}",
        "  {  \"spaced\"  :  \"value\"  ,  \"n\"  :  42  }  ",
        "{\"neg\":-123.456e-7,\"big\":12345678901234}",
        "{}",
        "[]",
        "{\"empty_str\":\"\",\"empty_arr\":[],\"empty_obj\":{}}",
    };

    public static void main(String[] args) throws Exception {
        int iterations = 300_000;
        int errors = 0;
        int total = 0;
        JSONParser parser = new JSONParser(JSONParser.MODE_JSON_SIMPLE);
        for (int iter = 0; iter < iterations; iter++) {
            for (String doc : DOCS) {
                total++;
                try {
                    Object parsed = parser.parse(doc);
                    if (parsed == null && !doc.trim().equals("null")) {
                        errors++;
                        if (errors <= 20) System.out.println("NULL parse at iter=" + iter + " doc=" + doc);
                        continue;
                    }
                    // Round-trip check: re-serialize and re-parse, compare
                    // string form -- catches silent value corruption, not
                    // just parse failures/exceptions.
                    String rt = JSONValue.toJSONString(parsed);
                    Object reparsed = parser.parse(rt);
                    String rt2 = JSONValue.toJSONString(reparsed);
                    if (!rt.equals(rt2)) {
                        errors++;
                        if (errors <= 20) {
                            System.out.println("ROUNDTRIP MISMATCH at iter=" + iter);
                            System.out.println("  doc=" + doc);
                            System.out.println("  rt1=" + rt);
                            System.out.println("  rt2=" + rt2);
                        }
                    }
                } catch (Exception ex) {
                    errors++;
                    if (errors <= 20) System.out.println("EXCEPTION at iter=" + iter + " doc=" + doc + " : " + ex);
                }
            }
            if (iter % 30000 == 0) System.out.println("iter=" + iter + " total=" + total + " errors=" + errors);
        }
        System.out.println("DONE total=" + total + " errors=" + errors);
        System.out.println(errors == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
