package org.apache.tomcat.websocket;

import java.io.IOException;
import java.util.concurrent.atomic.AtomicInteger;

import jakarta.websocket.OnClose;
import jakarta.websocket.OnError;
import jakarta.websocket.OnMessage;
import jakarta.websocket.OnOpen;
import jakarta.websocket.RemoteEndpoint.Basic;
import jakarta.websocket.Session;
import jakarta.websocket.server.ServerEndpoint;

import org.apache.tomcat.websocket.server.TesterEndpointConfig;

/**
 * Copy of the TesterFirehoseServer inline endpoint that reports its own send
 * progress, so a slow SEND can be told apart from a slow RECEIVE in one run.
 */
public class ProbeFirehoseServer {

    public static final String PATH = "/probefirehose";
    public static final int COUNT = Integer.getInteger("probe.count", TesterFirehoseServer.MESSAGE_COUNT).intValue();
    public static final AtomicInteger SENT = new AtomicInteger(0);
    public static final AtomicInteger ERRORS = new AtomicInteger(0);
    public static volatile String lastError = null;
    public static volatile long sendStartNanos = 0;
    public static volatile long sendEndNanos = 0;

    public static class ConfigInline extends TesterEndpointConfig {
        @Override
        protected Class<?> getEndpointClass() {
            return EndpointInline.class;
        }
    }

    @ServerEndpoint(PATH)
    public static class EndpointInline {

        private volatile boolean started = false;

        @OnOpen
        public void onOpen() {
            // no-op
        }

        @OnMessage
        public void onMessage(Session session, String msg) throws IOException {
            if (started) {
                return;
            }
            synchronized (this) {
                if (started) {
                    return;
                }
                started = true;
            }
            System.out.println("[server] Received " + msg + ", now sending data");
            System.out.flush();
            session.getUserProperties().put(Constants.BLOCKING_SEND_TIMEOUT_PROPERTY,
                    Long.valueOf(TesterFirehoseServer.SEND_TIME_OUT_MILLIS));
            Basic remote = session.getBasicRemote();
            remote.setBatchingAllowed(true);
            sendStartNanos = System.nanoTime();
            long t0 = sendStartNanos;
            try {
                for (int i = 0; i < COUNT; i++) {
                    remote.sendText(TesterFirehoseServer.MESSAGE);
                    int n = SENT.incrementAndGet();
                    if (n % 2000 == 0) {
                        System.out.printf("[server] sent=%d t=%.1fs%n", Integer.valueOf(n),
                                Double.valueOf((System.nanoTime() - t0) / 1e9));
                        System.out.flush();
                    }
                    if (i % (COUNT * 0.4) == 0) {
                        remote.setBatchingAllowed(false);
                        remote.setBatchingAllowed(true);
                    }
                }
            } catch (Throwable t) {
                ERRORS.incrementAndGet();
                lastError = t.toString();
                System.out.println("[server] SEND FAILED after " + SENT.get() + ": " + t);
                t.printStackTrace(System.out);
                System.out.flush();
                throw t;
            } finally {
                sendEndNanos = System.nanoTime();
            }
            System.out.printf("[server] send loop DONE sent=%d t=%.1fs%n", Integer.valueOf(SENT.get()),
                    Double.valueOf((sendEndNanos - t0) / 1e9));
            System.out.flush();
            session.close();
        }

        @OnError
        public void onError(Throwable t) {
            ERRORS.incrementAndGet();
            lastError = String.valueOf(t);
            System.out.println("[server] onError: " + t);
            t.printStackTrace(System.out);
            System.out.flush();
        }

        @OnClose
        public void onClose() {
            System.out.println("[server] onClose sent=" + SENT.get());
            System.out.flush();
        }
    }
}
