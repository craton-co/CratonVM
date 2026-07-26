import com.fasterxml.jackson.databind.ObjectMapper;
import com.fasterxml.jackson.dataformat.yaml.YAMLFactory;
import org.yaml.snakeyaml.Yaml;

import java.time.Instant;
import java.util.*;

public class SnakeYamlEmitProbe {
    // Mimics an Elasticsearch "interval provider" style document: nested maps,
    // lists, dates, numbers -- enough event variety to exercise Emitter.emit's
    // state machine repeatedly under a tight loop that crosses JIT thresholds.
    static Map<String, Object> buildDoc(int i) {
        Map<String, Object> doc = new LinkedHashMap<>();
        doc.put("name", "interval-" + i);
        doc.put("index", i);
        doc.put("enabled", i % 2 == 0);
        doc.put("created", Instant.ofEpochMilli(1700000000000L + i * 1000L).toString());
        List<Object> calendar = new ArrayList<>();
        for (int j = 0; j < 5; j++) {
            Map<String, Object> entry = new LinkedHashMap<>();
            entry.put("unit", j % 2 == 0 ? "minute" : "hour");
            entry.put("factor", j + i);
            calendar.add(entry);
        }
        doc.put("calendar", calendar);
        Map<String, Object> nested = new LinkedHashMap<>();
        nested.put("min", -i);
        nested.put("max", i * 3);
        nested.put("tags", Arrays.asList("a" + i, "b" + i, "c" + i));
        doc.put("range", nested);
        return doc;
    }

    public static void main(String[] args) throws Exception {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 8000;
        ObjectMapper yamlMapper = new ObjectMapper(new YAMLFactory());
        Yaml plainYaml = new Yaml();

        int failures = 0;
        int checked = 0;
        for (int i = 0; i < iterations; i++) {
            Map<String, Object> doc = buildDoc(i);

            // Path 1: Jackson YAML (matches the original ES/Jackson-YAML repro shape)
            String yamlText = yamlMapper.writeValueAsString(doc);
            @SuppressWarnings("unchecked")
            Map<String, Object> roundTrip = yamlMapper.readValue(yamlText, Map.class);

            // Path 2: plain SnakeYAML dump/load (direct Emitter.emit coverage)
            String plainText = plainYaml.dump(doc);
            @SuppressWarnings("unchecked")
            Map<String, Object> plainRoundTrip = (Map<String, Object>) plainYaml.load(plainText);

            checked++;
            if (i % 500 == 0) {
                System.out.println("progress i=" + i);
                System.out.flush();
            }
            if (!String.valueOf(doc.get("name")).equals(String.valueOf(roundTrip.get("name")))
                    || !String.valueOf(doc.get("index")).equals(String.valueOf(roundTrip.get("index")))) {
                failures++;
                if (failures <= 5) {
                    System.out.println("JACKSON-YAML MISMATCH at i=" + i + " doc=" + doc + " roundTrip=" + roundTrip);
                }
            }
            if (!String.valueOf(doc.get("name")).equals(String.valueOf(plainRoundTrip.get("name")))
                    || !String.valueOf(doc.get("index")).equals(String.valueOf(plainRoundTrip.get("index")))) {
                failures++;
                if (failures <= 5) {
                    System.out.println("SNAKEYAML MISMATCH at i=" + i + " doc=" + doc + " roundTrip=" + plainRoundTrip);
                }
            }
        }
        System.out.println("DONE checked=" + checked + " failures=" + failures);
        if (failures > 0) {
            System.exit(1);
        }
    }
}
