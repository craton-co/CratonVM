import java.lang.reflect.Field;
import java.net.URL;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSocketFactory;

/**
 * Diagnostic for the HttpsURLConnection per-connection setSSLSocketFactory
 * readback gap: what is the receiver our native actually sees, and does the
 * inherited javax.net.ssl.HttpsURLConnection.sslSocketFactory instance field
 * exist on it?
 *
 * In the real JDK the object is an HttpsURLConnectionImpl that DELEGATES to a
 * DelegateHttpsURLConnection, so the inherited field is not where the real
 * implementation keeps the factory. That is what decides whether storing the
 * factory in the real field is a sound implementation or whether a GC-rooted
 * side table is required.
 */
public class HucFactoryShapeProbe {
    public static void main(String[] args) throws Exception {
        URL url = new URL("https://repo.maven.apache.org/maven2/");
        HttpsURLConnection conn = (HttpsURLConnection) url.openConnection();

        System.out.println("receiverClass=" + conn.getClass().getName());
        for (Class<?> c = conn.getClass(); c != null; c = c.getSuperclass()) {
            System.out.println("  super: " + c.getName());
        }

        System.out.println("declared fields on javax.net.ssl.HttpsURLConnection:");
        for (Field f : HttpsURLConnection.class.getDeclaredFields()) {
            System.out.println("  " + f.getType().getSimpleName() + " " + f.getName());
        }

        System.out.println("declared fields on the receiver class:");
        for (Field f : conn.getClass().getDeclaredFields()) {
            System.out.println("  " + f.getType().getSimpleName() + " " + f.getName());
        }

        SSLContext c = SSLContext.getInstance("TLS");
        c.init(null, null, null);
        SSLSocketFactory mine = c.getSocketFactory();

        SSLSocketFactory before = conn.getSSLSocketFactory();
        System.out.println("getSSLSocketFactory before set = "
            + (before == null ? "null" : System.identityHashCode(before)));
        System.out.println("factory to install            = " + System.identityHashCode(mine));

        conn.setSSLSocketFactory(mine);
        SSLSocketFactory after = conn.getSSLSocketFactory();
        System.out.println("getSSLSocketFactory after set  = "
            + (after == null ? "null" : System.identityHashCode(after)));
        System.out.println("READBACK-OK=" + (after == mine));

        // Is the inherited field itself reachable/populated?
        try {
            Field f = HttpsURLConnection.class.getDeclaredField("sslSocketFactory");
            f.setAccessible(true);
            Object v = f.get(conn);
            System.out.println("inherited sslSocketFactory field = "
                + (v == null ? "null" : System.identityHashCode(v))
                + " (matches installed: " + (v == mine) + ")");
        } catch (NoSuchFieldException e) {
            System.out.println("inherited sslSocketFactory field = ABSENT");
        } catch (Throwable t) {
            System.out.println("inherited sslSocketFactory field = UNREADABLE: " + t);
        }

        // A second, independent connection must NOT see the first one's factory.
        HttpsURLConnection other =
            (HttpsURLConnection) new URL("https://repo.maven.apache.org/maven2/").openConnection();
        SSLSocketFactory otherFactory = other.getSSLSocketFactory();
        System.out.println("second connection factory      = "
            + (otherFactory == null ? "null" : System.identityHashCode(otherFactory)));
        System.out.println("ISOLATION-OK=" + (otherFactory != mine));
    }
}
