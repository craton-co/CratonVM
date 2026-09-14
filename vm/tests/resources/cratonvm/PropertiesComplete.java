// JAVA21+
package cratonvm;

import java.util.Properties;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;

/**
 * Session 22: Properties and Resource Loading.
 * System.getProperty, Properties.load, ClassLoader.getResourceAsStream, ResourceBundle.
 */
public class PropertiesComplete {

    // ---- Test 1: System.getProperty("os.name") ----
    public static int testOsName() {
        String os = System.getProperty("os.name");
        return (os != null && os.length() > 0) ? 1 : 0;
    }

    // ---- Test 2: System.getProperty("file.separator") ----
    public static int testFileSeparator() {
        String sep = System.getProperty("file.separator");
        return (sep != null && (sep.equals("/") || sep.equals("\\"))) ? 1 : 0;
    }

    // ---- Test 3: System.getProperty("line.separator") ----
    public static int testLineSeparator() {
        String sep = System.getProperty("line.separator");
        return (sep != null && sep.length() >= 1) ? 1 : 0;
    }

    // ---- Test 4: System.getProperty("path.separator") ----
    public static int testPathSeparator() {
        String sep = System.getProperty("path.separator");
        return (sep != null && (sep.equals(":") || sep.equals(";"))) ? 1 : 0;
    }

    // ---- Test 5: System.getProperty("user.dir") ----
    public static int testUserDir() {
        String dir = System.getProperty("user.dir");
        return (dir != null && dir.length() > 0) ? 1 : 0;
    }

    // ---- Test 6: System.getProperty("user.home") ----
    public static int testUserHome() {
        String home = System.getProperty("user.home");
        return (home != null && home.length() > 0) ? 1 : 0;
    }

    // ---- Test 7: System.getProperty("file.encoding") ----
    public static int testFileEncoding() {
        String enc = System.getProperty("file.encoding");
        return "UTF-8".equals(enc) ? 1 : 0;
    }

    // ---- Test 8: System.getProperty("java.version") ----
    public static int testJavaVersion() {
        String ver = System.getProperty("java.version");
        return (ver != null && ver.length() > 0) ? 1 : 0;
    }

    // ---- Test 9: System.getProperty("java.vendor") ----
    public static int testJavaVendor() {
        String vendor = System.getProperty("java.vendor");
        return "CratonVM".equals(vendor) ? 1 : 0;
    }

    // ---- Test 10: System.getProperty with default value ----
    public static int testGetPropertyDefault() {
        String val = System.getProperty("nonexistent.prop", "default_val");
        return "default_val".equals(val) ? 1 : 0;
    }

    // ---- Test 11: System.getProperty returns null for missing ----
    public static int testGetPropertyNull() {
        String val = System.getProperty("nonexistent.prop.xyz");
        return (val == null) ? 1 : 0;
    }

    // ---- Test 12: System.setProperty ----
    public static int testSetProperty() {
        System.setProperty("test.key", "test.value");
        String val = System.getProperty("test.key");
        return "test.value".equals(val) ? 1 : 0;
    }

    // ---- Test 13: System.setProperty returns old value ----
    public static int testSetPropertyReturnsOld() {
        System.setProperty("replace.key", "old");
        String old = System.setProperty("replace.key", "new");
        return "old".equals(old) ? 1 : 0;
    }

    // ---- Test 14: Properties constructor and setProperty/getProperty ----
    public static int testPropertiesBasic() {
        Properties props = new Properties();
        props.setProperty("key1", "value1");
        props.setProperty("key2", "value2");
        String v1 = props.getProperty("key1");
        String v2 = props.getProperty("key2");
        return ("value1".equals(v1) && "value2".equals(v2)) ? 1 : 0;
    }

    // ---- Test 15: Properties.getProperty with default ----
    public static int testPropertiesDefault() {
        Properties props = new Properties();
        String val = props.getProperty("missing", "fallback");
        return "fallback".equals(val) ? 1 : 0;
    }

    // ---- Test 16: Properties.load from InputStream ----
    public static int testPropertiesLoad() {
        String content = "app.name=TestApp\napp.version=2.0\n";
        ByteArrayInputStream bais = new ByteArrayInputStream(content.getBytes());
        Properties props = new Properties();
        try {
            props.load(bais);
        } catch (Exception e) {
            return 0;
        }
        String name = props.getProperty("app.name");
        String ver = props.getProperty("app.version");
        return ("TestApp".equals(name) && "2.0".equals(ver)) ? 1 : 0;
    }

    // ---- Test 17: Properties.load handles comments ----
    public static int testPropertiesLoadComments() {
        String content = "# comment\n! another comment\nkey1=val1\n";
        ByteArrayInputStream bais = new ByteArrayInputStream(content.getBytes());
        Properties props = new Properties();
        try {
            props.load(bais);
        } catch (Exception e) {
            return 0;
        }
        // Should have exactly 1 property, not the comments
        String v = props.getProperty("key1");
        return "val1".equals(v) ? 1 : 0;
    }

    // ---- Test 18: Properties.load with colon separator ----
    public static int testPropertiesLoadColon() {
        String content = "host: localhost\nport: 8080\n";
        ByteArrayInputStream bais = new ByteArrayInputStream(content.getBytes());
        Properties props = new Properties();
        try {
            props.load(bais);
        } catch (Exception e) {
            return 0;
        }
        String host = props.getProperty("host");
        String port = props.getProperty("port");
        return ("localhost".equals(host) && "8080".equals(port)) ? 1 : 0;
    }

    // ---- Test 19: Properties.size ----
    public static int testPropertiesSize() {
        Properties props = new Properties();
        props.setProperty("a", "1");
        props.setProperty("b", "2");
        props.setProperty("c", "3");
        return props.size();  // 3
    }

    // ---- Test 20: Properties.containsKey ----
    public static int testPropertiesContainsKey() {
        Properties props = new Properties();
        props.setProperty("exist", "yes");
        boolean has = props.containsKey("exist");
        boolean noHas = props.containsKey("nope");
        return (has && !noHas) ? 1 : 0;
    }

    // ---- Test 21: Properties.remove ----
    public static int testPropertiesRemove() {
        Properties props = new Properties();
        props.setProperty("key", "val");
        props.remove("key");
        return (props.getProperty("key") == null) ? 1 : 0;
    }

    // ---- Test 22: Properties overwrite ----
    public static int testPropertiesOverwrite() {
        Properties props = new Properties();
        props.setProperty("key", "first");
        props.setProperty("key", "second");
        return "second".equals(props.getProperty("key")) ? 1 : 0;
    }

    // ---- Test 23: Properties.isEmpty ----
    public static int testPropertiesEmpty() {
        Properties props = new Properties();
        boolean empty = props.isEmpty();
        props.setProperty("k", "v");
        boolean notEmpty = !props.isEmpty();
        return (empty && notEmpty) ? 1 : 0;
    }

    // ---- Test 24: Properties.clear ----
    public static int testPropertiesClear() {
        Properties props = new Properties();
        props.setProperty("a", "1");
        props.setProperty("b", "2");
        props.clear();
        return props.isEmpty() ? 1 : 0;
    }

    // ---- Test 25: System.lineSeparator ----
    public static int testSystemLineSeparator() {
        String sep = System.lineSeparator();
        return (sep != null && sep.length() >= 1) ? 1 : 0;
    }

    // ---- Test 26: System.getProperty("java.io.tmpdir") ----
    public static int testTmpDir() {
        String tmp = System.getProperty("java.io.tmpdir");
        return (tmp != null && tmp.length() > 0) ? 1 : 0;
    }

    // ---- Test 27: Properties.load empty input ----
    public static int testPropertiesLoadEmpty() {
        ByteArrayInputStream bais = new ByteArrayInputStream(new byte[0]);
        Properties props = new Properties();
        try {
            props.load(bais);
        } catch (Exception e) {
            return 0;
        }
        return props.isEmpty() ? 1 : 0;
    }

    // ---- Test 28: Properties.load with spaces ----
    //
    // `java.util.Properties.load` strips whitespace BEFORE the key, around the
    // separator, and after the key — but NOT after the value: everything from
    // the first non-whitespace value character to the end of the line is the
    // value, trailing spaces included (java.util.Properties javadoc, "the
    // characters following the key up to the end of the line"). So `  key1  =
    // value1  ` yields the value `"value1  "`, not `"value1"`.
    //
    // This test asserted `"value1".equals(v1)` and expected 1. Run under a real
    // JDK 25 it returns 0 — the expectation, not the VM, was wrong. Assert the
    // whole documented rule instead: leading trim, no trailing trim.
    public static int testPropertiesLoadSpaces() {
        String content = "  key1  =  value1  \nkey2=value2\n";
        ByteArrayInputStream bais = new ByteArrayInputStream(content.getBytes());
        Properties props = new Properties();
        try {
            props.load(bais);
        } catch (Exception e) {
            return 0;
        }
        String v1 = props.getProperty("key1");
        String v2 = props.getProperty("key2");
        return ("value1  ".equals(v1) && "value2".equals(v2)) ? 1 : 0;
    }

    // ---- Test 29: System.getenv returns value for known env var ----
    public static int testGetEnv() {
        // PATH or Path should exist on any platform
        String path = System.getenv("PATH");
        if (path == null) path = System.getenv("Path");
        return (path != null && path.length() > 0) ? 1 : 0;
    }

    // ---- Test 30: System.getenv returns null for missing ----
    public static int testGetEnvMissing() {
        String val = System.getenv("DEFINITELY_NONEXISTENT_VAR_XYZ_123");
        return (val == null) ? 1 : 0;
    }

    // ---- Test 31: ordinary Properties.get does not read System properties ----
    public static int testPlainPropertiesGetIgnoresSystemProperties() {
        String old = System.setProperty("password", "masked-global-password");
        try {
            Properties props = new Properties();
            if (props.get("password") != null) return 0;
            props.setProperty("password", "");
            return "".equals(props.get("password")) ? 1 : 0;
        } finally {
            if (old == null) System.clearProperty("password");
            else System.setProperty("password", old);
        }
    }

    // ---- Test 32: regex split used by Keycloak model parameter parsing ----
    public static int testStringSplitWhitespaceCommaRegex() {
        String[] parts = "Infinispan,Jpa".split("\\s*,\\s*");
        return parts.length == 2 && "Infinispan".equals(parts[0]) && "Jpa".equals(parts[1]) ? 1 : 0;
    }
}
