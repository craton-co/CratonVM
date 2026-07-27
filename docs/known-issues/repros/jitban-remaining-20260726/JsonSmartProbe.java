import net.minidev.json.JSONValue;
import net.minidev.json.JSONObject;
import net.minidev.json.JSONArray;

// JSONSMART-PARSER.1 (2026-07-09) repro: the ban's own note says the
// default JIT crashes inside emitted code after compiling parser cursor
// methods (JSONParserString.read/readS, JSONParserBase.skipSpace) --
// exercised via net.minidev.json.JSONValue.parse, which drives exactly
// this parser package for every call. Loops enough distinct JSON
// documents (varying whitespace/nesting/string-escape shapes to exercise
// skipSpace and both read paths) to cross the JIT compile threshold for
// these methods under real, repeated use.
public class JsonSmartProbe {
    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        String[] templates = {
            "{\"a\":1,\"b\":[1,2,3],\"c\":{\"d\":\"hello world\"}}",
            "  {  \"x\" : \"esc\\\"aped\\nvalue\" , \"y\" : [ 1 , 2 , 3 ]  }  ",
            "[1,2,3,4,5,6,7,8,9,10]",
            "{\"nested\":{\"deep\":{\"deeper\":{\"deepest\":\"value\"}}}}",
            "{\"unicode\":\"caf\\u00e9\",\"num\":3.14159,\"bool\":true,\"nil\":null}",
        };
        for (int i = 0; i < iterations; i++) {
            String json = templates[i % templates.length];
            Object parsed = JSONValue.parse(json);
            if (parsed == null) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- parse returned null for: " + json);
                System.exit(1);
            }
            String reserialized = JSONValue.toJSONString(parsed);
            Object reparsed = JSONValue.parse(reserialized);
            if (reparsed == null) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- round-trip re-parse failed for: " + reserialized);
                System.exit(1);
            }
            if (!(parsed instanceof JSONObject) && !(parsed instanceof JSONArray)) {
                System.out.println("RESULT: FAIL at iteration " + i + " -- unexpected parsed type: " + parsed.getClass());
                System.exit(1);
            }
        }
        System.out.println("RESULT: OK -- " + iterations + " parse+reserialize+reparse cycles, all consistent");
    }
}
