// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm.internal;

/**
 * The task object {@code HttpClient.sendAsync} hands to a new thread in the
 * {@code --jdk-only} build: the request runs on the thread that
 * {@code Runnable.run()} is entered from, and completes the future the caller
 * already holds.
 *
 * <p>The bytes of this class are compiled from this file and embedded in
 * {@code native-builtins/src/class_bytes/HttpSendTask.class};
 * {@code re5_start_async_send} defines it once per VM. It exists because the
 * fabricated carrier of the same name is refused under {@code --jdk-only}, and
 * the request then ran INLINE on the caller's thread: a caller that starts its
 * own server after {@code sendAsync} (Spring's
 * {@code WebClientIntegrationTests.malformedResponseChunks*}) never got there.
 *
 * <p>The four fields keep the slot order the native reads: future, client,
 * request, handler. {@code run()} is native, registered as
 * {@code re5_send_task_run}.
 *
 * <p>Rebuild with {@code javac --release 17 cratonvm/internal/HttpSendTask.java}.
 */
public final class HttpSendTask implements Runnable {

    Object future;
    Object client;
    Object request;
    Object handler;

    public HttpSendTask() {
    }

    @Override
    public native void run();
}
