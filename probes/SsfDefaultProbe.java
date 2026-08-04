import java.io.BufferedReader;
import java.io.InputStreamReader;
import java.io.OutputStream;
import java.net.InetSocketAddress;
import java.net.Socket;
import javax.net.ssl.SSLSocket;
import javax.net.ssl.SSLSocketFactory;

/**
 * Reproduces docs/known-issues/springboot/
 * sslsocketfactory-getdefault-aether-resolution-regression-20260804.md.
 *
 * Apache HttpClient's SSLConnectionSocketFactory.createLayeredSocket does
 * exactly this: take the factory handed back by the static
 * SSLSocketFactory.getDefault(), then call the layered
 * createSocket(Socket,String,int,boolean) overload on an already-connected
 * plain socket. Under the duplicate/stale native registration that path threw
 * IllegalStateException: SSLSocketFactory has no owning SSLContext
 * before any network I/O was attempted.
 */
public class SsfDefaultProbe {
    public static void main(String[] args) throws Exception {
        String host = args.length > 0 ? args[0] : "repo.maven.apache.org";
        int port = args.length > 1 ? Integer.parseInt(args[1]) : 443;
        String path = args.length > 2
            ? args[2]
            : "/maven2/com/google/code/gson/gson/2.10/gson-2.10.pom";

        SSLSocketFactory f = (SSLSocketFactory) SSLSocketFactory.getDefault();
        System.out.println("factory=" + f.getClass().getName());

        Socket plain = new Socket();
        plain.connect(new InetSocketAddress(host, port), 15000);
        plain.setSoTimeout(20000);
        System.out.println("plainConnected=" + plain.isConnected());

        Socket layered;
        try {
            layered = f.createSocket(plain, host, port, true);
        } catch (IllegalStateException e) {
            System.out.println("REPRO: " + e);
            e.printStackTrace(System.out);
            System.out.println("PROBE-RESULT=FAIL-NO-OWNING-CONTEXT");
            System.exit(2);
            return;
        }
        System.out.println("layered=" + layered.getClass().getName());

        SSLSocket ssl = (SSLSocket) layered;
        ssl.startHandshake();
        System.out.println("cipher=" + ssl.getSession().getCipherSuite());

        OutputStream out = ssl.getOutputStream();
        out.write(("GET " + path + " HTTP/1.1\r\nHost: " + host
                  + "\r\nConnection: close\r\nUser-Agent: ssf-probe\r\n\r\n")
                  .getBytes("US-ASCII"));
        out.flush();

        BufferedReader in = new BufferedReader(
            new InputStreamReader(ssl.getInputStream(), "US-ASCII"));
        String status = in.readLine();
        System.out.println("status=" + status);
        int bytes = 0;
        String line;
        while ((line = in.readLine()) != null) {
            bytes += line.length() + 1;
        }
        System.out.println("bodyBytes=" + bytes);
        ssl.close();

        if (status != null && status.startsWith("HTTP/1.1 200")) {
            System.out.println("PROBE-RESULT=PASS");
        } else {
            System.out.println("PROBE-RESULT=FAIL-BAD-STATUS");
            System.exit(3);
        }
    }
}
