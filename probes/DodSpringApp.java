// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// Definition-of-done driver: boot a real Spring Boot application, exercise
// it over HTTP or HTTPS, then close the context and RETURN NORMALLY.
//
// Returning normally is load-bearing: `--jdk-only-report` is not written when
// the program calls System.exit, and a report missing its
// `compatibility-class-requested` rows is exactly the file that reads as a
// clean run. A Spring Boot app is one `SpringApplication.exit` away from that,
// so this driver closes the context and falls off the end of main instead.
//
// Every line this prints is chosen by the program, never by the VM: status
// codes, byte counts and bean counts only. No identity hashes, no addresses,
// no thread names, no timings, no hash-container iteration order.

import java.io.InputStream;
import java.io.ByteArrayOutputStream;
import java.net.HttpURLConnection;
import java.net.URI;
import java.security.cert.X509Certificate;
import java.util.ArrayList;
import java.util.List;

import javax.net.ssl.HostnameVerifier;
import javax.net.ssl.HttpsURLConnection;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLSession;
import javax.net.ssl.TrustManager;
import javax.net.ssl.X509TrustManager;

import org.springframework.boot.SpringApplication;
import org.springframework.context.ConfigurableApplicationContext;

public final class DodSpringApp {

    public static void main(String[] args) throws Exception {
        String appClass = args[0];
        List<String> urls = new ArrayList<>();
        List<String> springArgs = new ArrayList<>();
        for (int i = 1; i < args.length; i++) {
            if (args[i].startsWith("http://") || args[i].startsWith("https://")) {
                urls.add(args[i]);
            } else {
                springArgs.add(args[i]);
            }
        }

        int failures = 0;
        ConfigurableApplicationContext ctx = null;
        try {
            SpringApplication app = new SpringApplication(Class.forName(appClass));
            app.setRegisterShutdownHook(false);
            ctx = app.run(springArgs.toArray(new String[0]));
            System.out.println("DOD CONTEXT-UP active=" + ctx.isActive()
                    + " beans=" + ctx.getBeanDefinitionCount());
            for (String u : urls) {
                if (!fetch(u)) {
                    failures++;
                }
            }
        } catch (Throwable t) {
            failures++;
            System.out.println("DOD THROWN " + t.getClass().getName() + ": " + t.getMessage());
            t.printStackTrace(System.out);
        } finally {
            if (ctx != null) {
                try {
                    ctx.close();
                    System.out.println("DOD CONTEXT-CLOSED");
                } catch (Throwable t) {
                    failures++;
                    System.out.println("DOD CLOSE-THREW " + t.getClass().getName() + ": " + t.getMessage());
                }
            }
        }
        System.out.println("DOD RESULT " + (failures == 0 ? "OK" : "FAILURES=" + failures));
    }

    /** One request. Prints the status code and the body length; never the body. */
    private static boolean fetch(String url) {
        // `url` may carry an expected status after a '#', e.g. ".../x#401".
        int expect = 200;
        String target = url;
        int hash = url.lastIndexOf('#');
        if (hash > 0) {
            expect = Integer.parseInt(url.substring(hash + 1));
            target = url.substring(0, hash);
        }
        try {
            HttpURLConnection c = (HttpURLConnection) URI.create(target).toURL().openConnection();
            if (c instanceof HttpsURLConnection) {
                HttpsURLConnection s = (HttpsURLConnection) c;
                s.setSSLSocketFactory(trustAll().getSocketFactory());
                s.setHostnameVerifier(new HostnameVerifier() {
                    @Override public boolean verify(String h, SSLSession sess) { return true; }
                });
            }
            c.setConnectTimeout(30000);
            c.setReadTimeout(60000);
            c.setInstanceFollowRedirects(false);
            int code = c.getResponseCode();
            InputStream in = (code >= 400) ? c.getErrorStream() : c.getInputStream();
            int n = 0;
            if (in != null) {
                ByteArrayOutputStream bos = new ByteArrayOutputStream();
                byte[] buf = new byte[8192];
                int r;
                while ((r = in.read(buf)) > 0) {
                    bos.write(buf, 0, r);
                }
                in.close();
                n = bos.size();
            }
            boolean ok = (code == expect);
            System.out.println("DOD FETCH " + shortName(target) + " status=" + code
                    + " expect=" + expect + " bytes>0=" + (n > 0) + " " + (ok ? "OK" : "MISMATCH"));
            c.disconnect();
            return ok;
        } catch (Throwable t) {
            System.out.println("DOD FETCH " + shortName(target) + " THREW "
                    + t.getClass().getName() + ": " + t.getMessage());
            return false;
        }
    }

    /** Scheme + path only: the port is chosen by the harness, not by the VM. */
    private static String shortName(String url) {
        try {
            URI u = URI.create(url);
            String p = u.getRawPath();
            return u.getScheme() + "://" + (p == null || p.isEmpty() ? "/" : p);
        } catch (Throwable t) {
            return "?";
        }
    }

    private static SSLContext trustAll() throws Exception {
        SSLContext sc = SSLContext.getInstance("TLS");
        sc.init(null, new TrustManager[] { new X509TrustManager() {
            @Override public void checkClientTrusted(X509Certificate[] c, String a) { }
            @Override public void checkServerTrusted(X509Certificate[] c, String a) { }
            @Override public X509Certificate[] getAcceptedIssuers() { return new X509Certificate[0]; }
        } }, new java.security.SecureRandom());
        return sc;
    }
}
