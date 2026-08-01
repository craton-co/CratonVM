import java.util.function.Consumer;

/**
 * LambdaMetafactory's contract: the SAM method of the generated class casts each
 * argument to the *instantiated* method type before calling the implementation
 * method. Invoking a `Listener<Narrow>` lambda through its erased SAM with a
 * wrong-typed event must therefore raise ClassCastException at the call
 * boundary — before the body runs.
 *
 * Spring depends on exactly that. `ApplicationListener.forPayload(Consumer)`
 * builds `event -> consumer.accept(event.getPayload())`, whose implementation
 * method takes `PayloadApplicationEvent`. The multicaster cannot resolve a
 * lambda's declared event type (a lambda's generic interfaces are raw), so it
 * invokes the listener for EVERY event and relies on
 * `SimpleApplicationEventMulticaster.doInvokeListener` catching the resulting
 * ClassCastException:
 *
 *     catch (ClassCastException ex) { ... "possibly a lambda-defined listener
 *     which we could not resolve the generic event type for" -> suppress }
 *
 * On CratonVM the body ran anyway with the wrong-typed argument, so the failure
 * surfaced as `NoSuchMethodError: ContextRefreshedEvent.getPayload()` instead —
 * which that catch does not cover, and which killed the context refresh in
 * DevToolsR2dbcAutoConfigurationTests$Pooled
 * .autoConfiguredInMemoryConnectionFactoryIsShutdown.
 *
 * The shape that matters is the DECLARING KIND of the lambda body:
 * `ApplicationListener` is an interface, so its `forPayload` factory and the
 * synthetic `lambda$forPayload$0` are static INTERFACE methods, and the
 * bootstrap method handle is an InterfaceMethodref. Case [A] below reproduces
 * that; case [B] is the same lambda declared on a class, as a control.
 */
public class LambdaSamArgCastProbe {

    /** Stands in for ApplicationEvent. */
    static class Event {

        final String name;

        Event(String name) {
            this.name = name;
        }
    }

    /** Stands in for PayloadApplicationEvent: adds the method the body calls. */
    static class PayloadEvent extends Event {

        private final Object payload;

        PayloadEvent(String name, Object payload) {
            super(name);
            this.payload = payload;
        }

        Object getPayload() {
            return this.payload;
        }
    }

    /** Stands in for ContextRefreshedEvent: an Event that is NOT a PayloadEvent. */
    static class OtherEvent extends Event {

        OtherEvent(String name) {
            super(name);
        }
    }

    /**
     * Stands in for ApplicationListener<E extends ApplicationEvent>, including
     * the static factory — so the lambda body is a static INTERFACE method and
     * the bootstrap handle is an InterfaceMethodref, exactly as in Spring.
     */
    interface Listener<E extends Event> {

        void onEvent(E event);

        static <T> Listener<PayloadEvent> forPayload(Consumer<T> consumer) {
            return (event) -> {
                @SuppressWarnings("unchecked")
                T payload = (T) event.getPayload();
                consumer.accept(payload);
            };
        }
    }

    /** Control: the identical lambda, but declared on a class. */
    static class ClassFactory {

        static <T> Listener<PayloadEvent> forPayload(Consumer<T> consumer) {
            return (event) -> {
                @SuppressWarnings("unchecked")
                T payload = (T) event.getPayload();
                consumer.accept(payload);
            };
        }
    }

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    @SuppressWarnings({ "rawtypes", "unchecked" })
    static void exercise(String label, Listener<PayloadEvent> listener, StringBuilder seen) {
        Listener raw = listener;

        seen.setLength(0);
        try {
            ((Listener<PayloadEvent>) raw).onEvent(new PayloadEvent("payload", "hello"));
            check(label + " correct arg delivered", "hello".contentEquals(seen), "seen=" + seen);
        }
        catch (Throwable ex) {
            check(label + " correct arg delivered", false, ex.getClass().getName() + ": " + ex.getMessage());
        }

        try {
            raw.onEvent(new OtherEvent("refreshed"));
            check(label + " wrong arg -> CCE", false, "no throwable at all — the body ran");
        }
        catch (ClassCastException ex) {
            check(label + " wrong arg -> CCE", true, "ClassCastException");
        }
        catch (Throwable ex) {
            check(label + " wrong arg -> CCE", false,
                    "expected ClassCastException, got " + ex.getClass().getName() + ": " + ex.getMessage());
        }

        seen.setLength(0);
        try {
            ((Listener<PayloadEvent>) raw).onEvent(new PayloadEvent("payload", "again"));
            check(label + " usable after CCE", "again".contentEquals(seen), "seen=" + seen);
        }
        catch (Throwable ex) {
            check(label + " usable after CCE", false, ex.getClass().getName() + ": " + ex.getMessage());
        }
    }

    public static void main(String[] args) {
        StringBuilder seen = new StringBuilder();

        System.out.println("[A] lambda body is a static INTERFACE method (the Spring shape)");
        exercise("[A]", Listener.forPayload((Object p) -> seen.append(p)), seen);

        System.out.println("[B] lambda body is a static CLASS method (control)");
        exercise("[B]", ClassFactory.forPayload((Object p) -> seen.append(p)), seen);

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }
}
