// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import com.sun.net.httpserver.HttpServer;
import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.HttpURLConnection;
import java.net.InetAddress;
import java.net.InetSocketAddress;
import java.net.URI;
import java.net.URL;
import java.net.http.HttpClient;
import java.net.http.HttpRequest;
import java.net.http.HttpResponse;
import java.nio.charset.StandardCharsets;

/**
 * SBR wave 5, lane {@code http}: loopback request/response throughput.
 *
 * <p>One {@code com.sun.net.httpserver.HttpServer} on 127.0.0.1 serving a
 * 1 KiB fixed-length body and an echo of a 1 KiB POST; driven by
 * {@code HttpURLConnection} (keep-alive, body fully read) and by a
 * {@code java.net.http.HttpClient} (HTTP/1.1). Each shape: a warm-up of
 * {@code warm} requests, then {@code n} timed requests; prints
 * {@code PERF <shape> us/req}. A checksum of the bodies is printed so a
 * broken arm cannot look fast.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . SbrW5HttpBench [n] [warm]
 * </pre>
 */
public class SbrW5HttpBench {
    static byte[] read(InputStream in) throws IOException {
        try (in) {
            ByteArrayOutputStream b = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int k;
            while ((k = in.read(buf)) >= 0) b.write(buf, 0, k);
            return b.toByteArray();
        }
    }

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 500;
        int warm = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        byte[] payload = new byte[1024];
        for (int i = 0; i < payload.length; i++) payload[i] = (byte) ('a' + i % 26);
        HttpServer server = HttpServer.create(new InetSocketAddress(InetAddress.getByName("127.0.0.1"), 0), 0);
        server.createContext("/get", ex -> {
            ex.sendResponseHeaders(200, payload.length);
            try (OutputStream o = ex.getResponseBody()) {
                o.write(payload);
            }
        });
        server.createContext("/post", ex -> {
            byte[] b = read(ex.getRequestBody());
            ex.sendResponseHeaders(200, b.length);
            try (OutputStream o = ex.getResponseBody()) {
                o.write(b);
            }
        });
        server.start();
        String base = "http://127.0.0.1:" + server.getAddress().getPort();
        URL get = new URL(base + "/get");
        URL post = new URL(base + "/post");
        HttpClient client = HttpClient.newBuilder().version(HttpClient.Version.HTTP_1_1).build();
        HttpRequest hget = HttpRequest.newBuilder(URI.create(base + "/get")).build();
        HttpRequest hpost = HttpRequest.newBuilder(URI.create(base + "/post"))
                .POST(HttpRequest.BodyPublishers.ofByteArray(payload)).build();

        String[] shapes = {"huc.get", "huc.post", "hc.get", "hc.post"};
        for (String shape : shapes) {
            long sum = 0;
            long t0 = 0;
            for (int i = 0; i < warm + n; i++) {
                if (i == warm) t0 = System.nanoTime();
                byte[] body;
                switch (shape) {
                    case "huc.get": {
                        HttpURLConnection c = (HttpURLConnection) get.openConnection();
                        body = read(c.getInputStream());
                        break;
                    }
                    case "huc.post": {
                        HttpURLConnection c = (HttpURLConnection) post.openConnection();
                        c.setDoOutput(true);
                        c.setFixedLengthStreamingMode(payload.length);
                        try (OutputStream o = c.getOutputStream()) {
                            o.write(payload);
                        }
                        body = read(c.getInputStream());
                        break;
                    }
                    case "hc.get":
                        body = client.send(hget, HttpResponse.BodyHandlers.ofByteArray()).body();
                        break;
                    default:
                        body = client.send(hpost, HttpResponse.BodyHandlers.ofByteArray()).body();
                        break;
                }
                sum += body.length + (body.length > 0 ? body[body.length - 1] : 0);
            }
            long us = (System.nanoTime() - t0) / 1000;
            System.out.println("PERF " + shape + " " + String.format("%.1f", (double) us / n) + " us/req check=" + sum);
        }
        server.stop(0);
        System.exit(0);
    }
}
