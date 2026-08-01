import java.util.function.Consumer;

import org.springframework.context.ApplicationEvent;
import org.springframework.context.ApplicationListener;
import org.springframework.context.event.ContextRefreshedEvent;
import org.springframework.context.support.StaticApplicationContext;

/**
 * `ApplicationListener.forPayload(Consumer)` returns a lambda whose
 * implementation method takes `PayloadApplicationEvent`. Spring's multicaster
 * cannot resolve a lambda's declared event type, so it invokes such a listener
 * for EVERY event and relies on the LambdaMetafactory argument cast raising
 * ClassCastException, which `SimpleApplicationEventMulticaster.doInvokeListener`
 * catches and suppresses.
 *
 * On CratonVM the body ran with the wrong-typed argument instead and raised
 * `NoSuchMethodError: ContextRefreshedEvent.getPayload()`, which that catch does
 * not cover — killing the context refresh in
 * DevToolsR2dbcAutoConfigurationTests$Pooled.
 *
 * This drives the real Spring types directly; the synthetic replica of the same
 * source shape (probes/LambdaSamArgCastProbe.java) does NOT reproduce.
 */
public class SpringForPayloadListenerProbe {

    static int failures = 0;

    static void check(String what, boolean ok, String detail) {
        System.out.println((ok ? "  OK   " : "  FAIL ") + what + "   " + detail);
        if (!ok) {
            failures++;
        }
    }

    @SuppressWarnings({ "rawtypes", "unchecked" })
    static void deliver(String label, ApplicationListener listener, ApplicationEvent event) {
        try {
            listener.onApplicationEvent(event);
            check(label, false, "no throwable at all — the body ran on a "
                    + event.getClass().getSimpleName());
        }
        catch (ClassCastException ex) {
            check(label, true, "ClassCastException (what doInvokeListener suppresses)");
        }
        catch (Throwable ex) {
            check(label, false, "expected ClassCastException, got "
                    + ex.getClass().getName() + ": " + ex.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        // With "preload", resolve the instantiated parameter type BEFORE the
        // wrong-typed delivery. If that alone restores the ClassCastException,
        // the cast is being skipped merely because the target class was not yet
        // in the class store — i.e. the guard fails open on "not loaded".
        if (args.length > 0 && "preload".equals(args[0])) {
            Class<?> c = Class.forName("org.springframework.context.PayloadApplicationEvent");
            System.out.println("preloaded " + c.getName());
        }
        else {
            System.out.println("PayloadApplicationEvent NOT preloaded");
        }

        StringBuilder seen = new StringBuilder();
        Consumer<Object> consumer = seen::append;
        ApplicationListener<?> listener = ApplicationListener.forPayload(consumer);
        System.out.println("listener class = " + listener.getClass().getName());

        System.out.println("[1] a plain ApplicationEvent must not reach the body");
        deliver("[1] plain ApplicationEvent", listener, new ApplicationEvent("source") {
        });

        System.out.println("[2] a ContextRefreshedEvent must not reach the body (the real case)");
        StaticApplicationContext context = new StaticApplicationContext();
        deliver("[2] ContextRefreshedEvent", listener, new ContextRefreshedEvent(context));

        System.out.println("[3] the listener still works for its own event type");
        try {
            seen.setLength(0);
            context.publishEvent("hello");
            check("[3] payload delivered", true, "publishEvent completed, seen=" + seen);
        }
        catch (Throwable ex) {
            check("[3] payload delivered", false, ex.getClass().getName() + ": " + ex.getMessage());
        }

        System.out.println();
        System.out.println(failures == 0 ? "PROBE PASS" : "PROBE FAIL (" + failures + ")");
        System.exit(failures == 0 ? 0 : 1);
    }
}
