import java.io.InputStream;
import java.net.URL;

/** ByteBuddy's TypePool locates class files through the loader's RESOURCE
 *  surface, not through loadClass. Mockito's inline mock maker resolves the
 *  whole type hierarchy that way, so one unreadable .class resource becomes
 *  "IllegalArgumentException: Unknown type: <internal name>". */
public class ClassBytesProbe {
    static void check(String binary) {
        String res = binary.replace('.', '/') + ".class";
        ClassLoader cl = ClassBytesProbe.class.getClassLoader();
        String viaStream, viaUrl, viaClassRes;
        int n = -1;
        try (InputStream in = cl.getResourceAsStream(res)) {
            if (in == null) viaStream = "NULL";
            else { n = in.readAllBytes().length; viaStream = "ok(" + n + " bytes)"; }
        } catch (Throwable t) { viaStream = "THREW " + t; }
        URL u = cl.getResource(res);
        viaUrl = (u == null) ? "NULL" : "ok";
        try {
            Class<?> c = Class.forName(binary, false, cl);
            InputStream in2 = c.getResourceAsStream("/" + res);
            viaClassRes = (in2 == null) ? "NULL" : "ok";
            if (in2 != null) in2.close();
        } catch (Throwable t) { viaClassRes = "THREW " + t.getClass().getSimpleName(); }
        System.out.printf("  %-72s stream=%-16s url=%-5s classRes=%s%n",
                binary.substring(binary.lastIndexOf('.') + 1), viaStream, viaUrl, viaClassRes);
    }
    public static void main(String[] a) {
        String[] names = {
            "org.springframework.test.context.bean.override.mockito.hierarchies.FooService",
            "org.springframework.test.context.bean.override.mockito.integration.SpringExtensionAndMockitoExtensionIntegrationTests",
            "org.springframework.test.context.aot.AotIntegrationTests",
            "java.lang.String",
        };
        for (String n : names) check(n);
    }
}
