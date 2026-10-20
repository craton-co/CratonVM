// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import javax.net.ssl.SSLContext;
import javax.net.ssl.SSLEngine;
import javax.net.ssl.SSLEngineResult;

/**
 * gci i22/fl: a TLS-heavy heap for {@code CRATONVM_GC_TLS_SPAN_DROP} (dev zr triage-t1:
 * "keep OFF -- sound; no TLS-heavy cost row").
 *
 * <p>Creates {@code n} {@link SSLEngine}s from one context (no handshake), keeps the
 * last {@code ring} of them alive in a ring (so they age into the old generation and die
 * there, which is what a concurrent sweep's freed spans meet) and allocates young
 * garbage beside each. Every engine is an owner row of the VM's TLS tables when the TLS
 * natives back it; the switch only changes how a sweep slice drops the dead ones.
 *
 * <p>Usage: {@code GciFl22TlsEngineChurnProbe <n> <ring>}. Stdout is deterministic
 * (counts and a port sum); {@code [fl22-tls] ms=} on stderr.
 */
public class GciFl22TlsEngineChurnProbe {
    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 200_000;
        int ring = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        SSLContext ctx = SSLContext.getInstance("TLS");
        ctx.init(null, null, null);
        SSLEngine[] kept = new SSLEngine[ring];
        long t0 = System.nanoTime();
        int client = 0;
        long portSum = 0;
        for (int i = 0; i < n; i++) {
            SSLEngine e = ctx.createSSLEngine("host" + (i % 97), 443 + (i % 7));
            e.setUseClientMode((i & 1) == 0);
            if (e.getUseClientMode()) {
                client++;
            }
            portSum += e.getPeerPort();
            kept[i % ring] = e;
            for (int j = 0; j < 16; j++) {
                sink = new byte[512];
            }
        }
        int idle = 0;
        for (SSLEngine e : kept) {
            if (e != null && e.getHandshakeStatus() == SSLEngineResult.HandshakeStatus.NOT_HANDSHAKING) {
                idle++;
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("tls-engine-churn n=" + n + " ring=" + ring + " client=" + client
                + " portsum=" + portSum + " kept-not-handshaking=" + idle);
        System.err.println("[fl22-tls] ms=" + ms);
        System.out.println("PROBE-OK");
    }
}
