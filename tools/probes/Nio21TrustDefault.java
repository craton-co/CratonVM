// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.URI;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import javax.net.ssl.SSLContext;

/**
 * nio21 wave 13 (L4): {@code SSLContext.init(null, null, null)} is SunJSSE's default trust manager
 * ({@code cacerts}), the same set {@code TrustManagerFactory.init((KeyStore) null)} answers. One
 * HTTPS GET to a public site through each spelling must succeed (the public CAs are in
 * {@code cacerts}); the run needs outbound network. Facts only, one line each; compare with
 * {@code java -cp . Nio21TrustDefault} and with {@code -Djavax.net.ssl.trustStore=<empty.p12>}
 * (every spelling must then fail the handshake the same way).
 */
public class Nio21TrustDefault {
    static String get(SSLContext c) {
        try {
            HttpClient h = HttpClient.newBuilder().sslContext(c)
                    .connectTimeout(java.time.Duration.ofSeconds(20)).build();
            HttpResponse<Void> r = h.send(HttpRequest.newBuilder(URI.create("https://example.com/"))
                    .timeout(java.time.Duration.ofSeconds(30)).build(), HttpResponse.BodyHandlers.discarding());
            return "status " + (r.statusCode() / 100) + "xx";
        } catch (Throwable t) {
            Throwable r = t;
            while (r.getCause() != null) r = r.getCause();
            return "!" + t.getClass().getName() + "/" + r.getClass().getName();
        }
    }

    public static void main(String[] a) throws Exception {
        SSLContext c1 = SSLContext.getInstance("TLS");
        c1.init(null, null, null);
        System.out.println("init-null " + get(c1));
        System.out.println("getDefault " + get(SSLContext.getDefault()));
        javax.net.ssl.TrustManagerFactory f = javax.net.ssl.TrustManagerFactory.getInstance(
                javax.net.ssl.TrustManagerFactory.getDefaultAlgorithm());
        f.init((java.security.KeyStore) null);
        SSLContext c2 = SSLContext.getInstance("TLS");
        c2.init(null, f.getTrustManagers(), null);
        System.out.println("tmf-null " + get(c2));
    }
}
