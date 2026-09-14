import java.net.*;
import java.io.*;

/** The java.net.HttpURLConnection accessor surface, WITHOUT connecting.
 *
 *  This is the overlap between the two registrars that drift on it:
 *  phases_early.rs::register_phase54_net_extras (synthetic-only) and
 *  http_url_connection.rs::register_one (shipping). Only the shipping one
 *  exists in a shipping binary, so HotSpot is the oracle for what it must do.
 *
 *  No network: openConnection() does not connect, and every method here is
 *  documented to work before connect(). */
public class HucAccessors {
    static int checks = 0, bad = 0;
    static void eq(String what, Object got, Object want) {
        checks++;
        boolean ok = (want == null) ? got == null : want.equals(got);
        if (!ok) { bad++; System.out.println("  DIFF " + what + ": got=" + got + " want=" + want); }
        else System.out.println("  ok   " + what + " = " + got);
    }
    static void throwsIae(String what, Runnable r) {
        checks++;
        try { r.run(); bad++; System.out.println("  DIFF " + what + " did not throw"); }
        catch (IllegalArgumentException e) { System.out.println("  ok   " + what + " threw IAE"); }
        catch (Throwable t) { bad++; System.out.println("  DIFF " + what + " threw " + t.getClass().getName()); }
    }

    public static void main(String[] a) throws Exception {
        URL u = new URL("http://127.0.0.1:1/path?q=1");
        HttpURLConnection c = (HttpURLConnection) u.openConnection();

        eq("getURL()", c.getURL(), u);
        eq("getRequestMethod() default", c.getRequestMethod(), "GET");

        // defaults, straight from the JDK's own field initialisers
        eq("getDoInput() default", c.getDoInput(), Boolean.TRUE);
        eq("getDoOutput() default", c.getDoOutput(), Boolean.FALSE);
        eq("getUseCaches() default", c.getUseCaches(), Boolean.TRUE);
        eq("getInstanceFollowRedirects() default", c.getInstanceFollowRedirects(), Boolean.TRUE);
        eq("getConnectTimeout() default", c.getConnectTimeout(), Integer.valueOf(0));
        eq("getReadTimeout() default", c.getReadTimeout(), Integer.valueOf(0));

        // round-trips
        c.setDoInput(false);   eq("getDoInput after set(false)", c.getDoInput(), Boolean.FALSE);
        c.setDoInput(true);    eq("getDoInput after set(true)", c.getDoInput(), Boolean.TRUE);
        c.setDoOutput(true);   eq("getDoOutput after set(true)", c.getDoOutput(), Boolean.TRUE);
        c.setUseCaches(false); eq("getUseCaches after set(false)", c.getUseCaches(), Boolean.FALSE);
        c.setInstanceFollowRedirects(false);
        eq("getInstanceFollowRedirects after set(false)", c.getInstanceFollowRedirects(), Boolean.FALSE);
        c.setConnectTimeout(1234); eq("getConnectTimeout after set", c.getConnectTimeout(), Integer.valueOf(1234));
        c.setReadTimeout(5678);    eq("getReadTimeout after set", c.getReadTimeout(), Integer.valueOf(5678));

        // setRequestMethod round-trip and its contract
        c.setRequestMethod("POST"); eq("getRequestMethod after POST", c.getRequestMethod(), "POST");
        c.setRequestMethod("HEAD"); eq("getRequestMethod after HEAD", c.getRequestMethod(), "HEAD");
        checks++;
        try { c.setRequestMethod("BOGUS"); bad++; System.out.println("  DIFF setRequestMethod(BOGUS) did not throw"); }
        catch (ProtocolException e) { System.out.println("  ok   setRequestMethod(BOGUS) threw ProtocolException"); }
        catch (Throwable t) { bad++; System.out.println("  DIFF setRequestMethod(BOGUS) threw " + t.getClass().getName()); }
        eq("getRequestMethod unchanged after refusal", c.getRequestMethod(), "HEAD");

        // request properties
        c.setRequestProperty("X-A", "1");
        eq("getRequestProperty(X-A)", c.getRequestProperty("X-A"), "1");
        c.setRequestProperty("X-A", "2");
        eq("setRequestProperty overwrites", c.getRequestProperty("X-A"), "2");
        c.addRequestProperty("X-A", "3");
        // MEASURED on HotSpot 25.0.3+9, not assumed: on a PRE-CONNECT request
        // header, getRequestProperty answers the LAST value added, not a
        // comma-joined list. The joined form is what getRequestProperties (and
        // a RESPONSE header) gives. My first draft expected "2, 3" and the
        // oracle said "3" -- the probe was wrong, not the VM.
        eq("addRequestProperty -> last value wins", c.getRequestProperty("X-A"), "3");
        eq("getRequestProperty is case-insensitive", c.getRequestProperty("x-a"), "3");
        eq("getRequestProperty(absent)", c.getRequestProperty("X-Nope"), null);
        checks++;
        java.util.Map<String, java.util.List<String>> rp = c.getRequestProperties();
        if (rp != null && rp.containsKey("X-A")) System.out.println("  ok   getRequestProperties has X-A = " + rp.get("X-A"));
        else { bad++; System.out.println("  DIFF getRequestProperties missing X-A: " + rp); }

        // negative timeouts are an IAE on the real JDK
        throwsIae("setConnectTimeout(-1)", () -> c.setConnectTimeout(-1));
        throwsIae("setReadTimeout(-1)", () -> c.setReadTimeout(-1));

        // streaming modes are mutually exclusive and reject bad values
        HttpURLConnection s1 = (HttpURLConnection) u.openConnection();
        s1.setFixedLengthStreamingMode(100);
        checks++;
        try { s1.setChunkedStreamingMode(10); bad++;
              System.out.println("  DIFF chunked after fixed did not throw"); }
        catch (IllegalStateException e) { System.out.println("  ok   chunked after fixed threw ISE"); }
        catch (Throwable t) { bad++; System.out.println("  DIFF chunked after fixed threw " + t.getClass().getName()); }
        throwsIae("setFixedLengthStreamingMode(-1)", () -> {
            HttpURLConnection s2 = null;
            try { s2 = (HttpURLConnection) u.openConnection(); } catch (IOException e) { throw new RuntimeException(e); }
            s2.setFixedLengthStreamingMode(-1); });

        eq("usingProxy() before connect", c.usingProxy(), Boolean.FALSE);

        System.out.println(bad == 0 ? "PASS HucAccessors (" + checks + " checks)"
                                    : "FAIL HucAccessors (" + bad + " of " + checks + " wrong)");
    }
}
