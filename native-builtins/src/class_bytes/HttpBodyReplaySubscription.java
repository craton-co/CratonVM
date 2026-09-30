// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm.net;

import java.nio.ByteBuffer;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Flow;

/**
 * One-shot replay {@code Flow.Subscription} that the {@code --jdk-only} build of
 * {@code HttpClient.send} hands to a real {@code BodySubscriber}.
 *
 * <p>The bytes of this class are compiled from this file and embedded in
 * {@code native-builtins/src/class_bytes/HttpBodyReplaySubscription.class};
 * {@code re5_drive_body_handler} defines it once per VM. It exists because no
 * public JDK class delivers a body synchronously, on the requesting thread, at
 * the first {@code request(n)} -- see
 * {@code docs/internal/jdk-only/W7-24-httpserverloop-and-strict-fallbacks.md} section 4.
 *
 * <p>Rebuild with {@code javac --release 17 cratonvm/net/HttpBodyReplaySubscription.java}.
 */
public final class HttpBodyReplaySubscription implements Flow.Subscription {

    private Flow.Subscriber<? super List<ByteBuffer>> subscriber;
    private byte[] body;
    /** 0 = pending, 1 = delivered, 2 = cancelled. */
    private int state;

    public HttpBodyReplaySubscription(Flow.Subscriber<? super List<ByteBuffer>> subscriber, byte[] body) {
        this.subscriber = subscriber;
        this.body = body;
    }

    @Override
    public void request(long n) {
        Flow.Subscriber<? super List<ByteBuffer>> s;
        byte[] b;
        // Claim delivery BEFORE invoking the subscriber: onNext commonly
        // re-enters request (HttpResponseInputStream asks for the next list
        // while it consumes the current one).
        synchronized (this) {
            if (n <= 0 || state != 0) {
                return;
            }
            state = 1;
            s = subscriber;
            b = body;
            subscriber = null;
            body = null;
        }
        if (s == null) {
            return;
        }
        if (b != null && b.length > 0) {
            s.onNext(Collections.singletonList(ByteBuffer.wrap(b)));
        }
        s.onComplete();
    }

    @Override
    public void cancel() {
        synchronized (this) {
            if (state == 0) {
                state = 2;
                subscriber = null;
                body = null;
            }
        }
    }
}
