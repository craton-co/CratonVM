// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.URI;
import java.net.URL;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLServerSocketFactory;
import javax.net.ssl.SSLSocketFactory;

/**
 * nio21 wave 13 (L4): the concrete classes of the TLS / HTTP objects a program is handed, one line
 * each. HotSpot answers the JDK's own implementation classes ({@code sun.security.ssl.*},
 * {@code jdk.internal.net.http.*}, {@code sun.net.httpserver.*}); a carrier that is an instance of
 * the ABSTRACT class answers the public type. Compare with {@code java -cp . Nio21CarrierClasses}.
 */
public class Nio21CarrierClasses {
    interface T { Object get() throws Throwable; }

    static void row(String name, T t) {
        String v;
        try {
            Object o = t.get();
            v = o == null ? "null" : o.getClass().getName();
        } catch (Throwable x) {
            v = "!" + x.getClass().getName();
        }
        System.out.println(name + " " + v);
    }

    public static void main(String[] a) throws Exception {
        SSLContext c = SSLContext.getDefault();
        row("sslcontext", () -> c);
        row("sslsocketfactory", () -> SSLSocketFactory.getDefault());
        row("sslserversocketfactory", () -> SSLServerSocketFactory.getDefault());
        row("sslsocket", () -> ((SSLSocketFactory) SSLSocketFactory.getDefault()).createSocket());
        row("sslserversocket", () -> ((SSLServerSocketFactory) SSLServerSocketFactory.getDefault()).createServerSocket());
        SSLEngine e = c.createSSLEngine();
        row("sslengine", () -> e);
        row("sslsession", () -> e.getSession());
        row("handshakesession", () -> e.getHandshakeSession());
        HttpClient h = HttpClient.newHttpClient();
        row("httpclient", () -> h);
        row("httpclient-builder", () -> HttpClient.newBuilder());
        row("httprequest-builder", () -> HttpRequest.newBuilder(URI.create("http://localhost/")));
        row("httprequest", () -> HttpRequest.newBuilder(URI.create("http://localhost/")).build());
        row("httpserver", () -> com.sun.net.httpserver.HttpServer.create());
        row("urlconnection-http", () -> new URL("http://localhost/").openConnection());
        row("urlconnection-https", () -> new URL("https://localhost/").openConnection());
        row("runnable-task", () -> e.getDelegatedTask());
    }
}
