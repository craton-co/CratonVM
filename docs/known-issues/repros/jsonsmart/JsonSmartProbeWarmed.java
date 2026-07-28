import net.minidev.json.JSONValue;
import net.minidev.json.JSONObject;
import net.minidev.json.JSONArray;
import net.minidev.json.parser.JSONParser;

// Same stress body as docs/known-issues/repros/jsonsmart/JsonSmartProbe.java,
// plus a warm-up that first drives a handful of *failing* parses. That matters
// for JIT coverage: every hot JSONParserBase/JSONParserString method contains a
// `new net.minidev.json.parser.ParseException(...)` on its error path, and the
// JIT refuses to compile a method whose `new` target class is not loaded yet
// (cp_new_resolver -> None -> whole-compile bail, three strikes -> the method
// interprets forever). Without the warm-up the parser package is never actually
// JIT-compiled, so a "no corruption" result would prove nothing about the JIT.
public class JsonSmartProbeWarmed {
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

    static final String[] BAD_DOCS = {
        "{\"a\":}",
        "[1,2,",
        "{\"unterminated\":\"abc",
        "tru",
        "{\"k\" \"v\"}",
    };

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 300_000;
        int errors = 0;
        int total = 0;
        JSONParser parser = new JSONParser(JSONParser.MODE_JSON_SIMPLE);
        int warmupThrows = 0;
        for (String bad : BAD_DOCS) {
            try {
                parser.parse(bad);
            } catch (Exception expected) {
                warmupThrows++;
            }
        }
        System.out.println("warmup: " + warmupThrows + "/" + BAD_DOCS.length + " expected parse failures");
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
            if (iter % 1000 == 0) System.out.println("iter=" + iter + " total=" + total + " errors=" + errors);
        }
        System.out.println("DONE total=" + total + " errors=" + errors);
        System.out.println(errors == 0 ? "PROBE_PASS" : "PROBE_FAIL");
    }
}
