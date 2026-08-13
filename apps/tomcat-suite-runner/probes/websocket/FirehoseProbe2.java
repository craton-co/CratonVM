package org.apache.tomcat.websocket;

import java.net.URI;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.TimeUnit;

import jakarta.websocket.ClientEndpointConfig;
import jakarta.websocket.ContainerProvider;
import jakarta.websocket.Session;
import jakarta.websocket.WebSocketContainer;

import org.junit.Test;

import org.apache.catalina.Context;
import org.apache.catalina.servlets.DefaultServlet;
import org.apache.catalina.startup.Tomcat;
import org.apache.tomcat.websocket.TesterMessageCountClient.BasicText;
import org.apache.tomcat.websocket.TesterMessageCountClient.TesterProgrammaticEndpoint;

/** Firehose with BOTH sides instrumented. */
public class FirehoseProbe2 extends WebSocketBaseTest {

    @Test
    public void probe() throws Exception {
        Tomcat tomcat = getTomcatInstance();
        Context ctx = getProgrammaticRootContext();
        ctx.addApplicationListener(ProbeFirehoseServer.ConfigInline.class.getName());
        Tomcat.addServlet(ctx, "default", new DefaultServlet());
        ctx.addServletMappingDecoded("/", "default");

        tomcat.start();

        WebSocketContainer wsContainer = ContainerProvider.getWebSocketContainer();
        ClientEndpointConfig cfg = ClientEndpointConfig.Builder.create().build();

        Session wsSession = wsContainer.connectToServer(TesterProgrammaticEndpoint.class, cfg,
                new URI("ws://localhost:" + getPort() + ProbeFirehoseServer.PATH));
        CountDownLatch latch = new CountDownLatch(ProbeFirehoseServer.COUNT);
        BasicText handler = new BasicText(latch, TesterFirehoseServer.MESSAGE);
        wsSession.addMessageHandler(handler);

        final long t0 = System.nanoTime();
        Thread mon = new Thread(() -> {
            int last = 0;
            int lastSent = 0;
            long lastT = t0;
            try {
                while (true) {
                    Thread.sleep(2000);
                    int now = handler.getMessageCount();
                    int sent = ProbeFirehoseServer.SENT.get();
                    long t = System.nanoTime();
                    double dt = (t - lastT) / 1e9;
                    System.out.printf("[probe] t=%.1fs recv=%d (+%d, %.0f/s) sent=%d (+%d, %.0f/s) open=%s err=%d%n",
                            Double.valueOf((t - t0) / 1e9), Integer.valueOf(now), Integer.valueOf(now - last),
                            Double.valueOf((now - last) / dt), Integer.valueOf(sent),
                            Integer.valueOf(sent - lastSent), Double.valueOf((sent - lastSent) / dt),
                            Boolean.valueOf(wsSession.isOpen()),
                            Integer.valueOf(ProbeFirehoseServer.ERRORS.get()));
                    System.out.flush();
                    last = now;
                    lastSent = sent;
                    lastT = t;
                }
            } catch (InterruptedException e) {
                // done
            }
        });
        mon.setDaemon(true);
        mon.start();

        wsSession.getBasicRemote().sendText("Hello");
        boolean done = handler.getLatch().await(TesterFirehoseServer.WAIT_TIME_MILLIS, TimeUnit.MILLISECONDS);
        mon.interrupt();
        System.out.printf("[probe] DONE latch=%s recv=%d sent=%d wall=%.1fs open=%s errors=%d lastError=%s%n",
                Boolean.valueOf(done), Integer.valueOf(handler.getMessageCount()),
                Integer.valueOf(ProbeFirehoseServer.SENT.get()), Double.valueOf((System.nanoTime() - t0) / 1e9),
                Boolean.valueOf(wsSession.isOpen()), Integer.valueOf(ProbeFirehoseServer.ERRORS.get()),
                ProbeFirehoseServer.lastError);
        System.out.flush();
    }
}
