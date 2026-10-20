import java.util.HashMap;
import java.util.Properties;
import java.util.concurrent.ConcurrentHashMap;

/** Exercises repeated virtual dispatch on real JDK map storage in both modes. */
public class FfmRealMapDispatch {
    public static class Constructed { public int value = 7; }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 3; i++) {
            if (Constructed.class.newInstance().value != 7) throw new AssertionError("cached constructor");
        }
        if (!java.nio.charset.Charset.forName("windows-1252").name().equals("windows-1252"))
            throw new AssertionError("charset constructor");
        String separator = System.getProperties().getProperty("file.separator");
        if (!java.io.File.separator.equals(separator)) throw new AssertionError("bootstrap property");
        Properties properties = new Properties();
        HashMap<String, String> map = new HashMap<>();
        ConcurrentHashMap<String, String> concurrent = new ConcurrentHashMap<>();
        for (int i = 0; i < 200; i++) {
            String value = Integer.toString(i);
            properties.put("key", value);
            map.put("key", value);
            concurrent.remove("key");
            if (concurrent.putIfAbsent("key", value) != null) throw new AssertionError("putIfAbsent");
            if (!value.equals(properties.getProperty("key")) || !value.equals(map.get("key"))
                    || !value.equals(concurrent.get("key"))) throw new AssertionError("virtual cache " + i);
        }
        System.out.println("bootstrapAndWarmMapDispatch=true");
        System.out.println("PASS");
    }
}
