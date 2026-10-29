// Declares the package its directory implies. Without it javac staged this
// hand-run probe in the DEFAULT package while the file lived under
// `cratonvm/`, so its `this_class` contradicted its path — the shape HotSpot
// rejects outright and only CratonVM's loader tolerated. No corpus test
// invokes it under a package name today; the declaration is here so one can.
package cratonvm;

import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.nio.charset.StandardCharsets;
import java.util.jar.Attributes;
import java.util.jar.Manifest;
import java.util.LinkedHashMap;
import java.util.Map;

/**
 * HotSpot preserves a manifest main-attribute mapping whose value is null and
 * serializes that value as the literal text "null". Spring Boot loader-tools
 * deliberately depends on that behaviour when its package version is absent.
 */
public class ManifestNullValue {
    public static void main(String[] args) throws Exception {
        Map<String, String> directMap = new LinkedHashMap<>();
        directMap.put("direct-null", null);
        if (!directMap.containsKey("direct-null")) {
            throw new AssertionError("LinkedHashMap did not retain a null-valued mapping");
        }
        directMap.put(null, null);
        if (!directMap.containsKey(null)) {
            throw new AssertionError("LinkedHashMap did not retain a null-key/null-valued mapping");
        }
        Map<Attributes.Name, String> namedMap = new LinkedHashMap<>();
        Attributes.Name directName = new Attributes.Name("Spring-Boot-Version");
        namedMap.put(directName, null);
        if (!namedMap.containsKey(directName)) {
            throw new AssertionError("LinkedHashMap did not retain an Attributes.Name null-valued mapping");
        }

        Manifest manifest = new Manifest();
        Attributes attributes = manifest.getMainAttributes();
        attributes.putValue("Manifest-Version", "1.0");
        if (!attributes.containsKey(new Attributes.Name("Manifest-Version"))) {
            throw new AssertionError("non-null manifest attribute was not retained");
        }
        attributes.putValue("Spring-Boot-Version", null);

        Attributes.Name version = new Attributes.Name("Spring-Boot-Version");
        if (attributes.size() != 2) {
            throw new AssertionError("null-valued manifest attribute did not increase size: " + attributes.size());
        }
        if (!attributes.containsKey(version)) {
            throw new AssertionError("null-valued manifest attribute was not retained");
        }
        if (attributes.getValue(version) != null) {
            throw new AssertionError("null-valued manifest attribute did not read back as null");
        }
        Attributes.Name objectNull = new Attributes.Name("Object-Null");
        attributes.put(objectNull, null);
        if (attributes.size() != 3 || !attributes.containsKey(objectNull)) {
            throw new AssertionError("Attributes.put did not retain a null-valued mapping");
        }

        ByteArrayOutputStream out = new ByteArrayOutputStream();
        manifest.write(out);
        byte[] bytes = out.toByteArray();
        String text = new String(bytes, StandardCharsets.UTF_8);
        if (!text.contains("Spring-Boot-Version: null")) {
            throw new AssertionError("manifest serialization dropped null-valued attribute: " + text);
        }
        if (!text.contains("Object-Null: null")) {
            throw new AssertionError("manifest serialization dropped put(Object, null): " + text);
        }

        Manifest reparsed = new Manifest(new ByteArrayInputStream(bytes));
        if (!reparsed.getMainAttributes().containsKey(version)) {
            throw new AssertionError("reparsed manifest lost null-valued attribute");
        }
        if (!"null".equals(reparsed.getMainAttributes().getValue(version))) {
            throw new AssertionError("reparsed null-valued attribute was not literal null");
        }
        System.out.println("OK manifest-null-value");
    }
}
