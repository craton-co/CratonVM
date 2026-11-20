import java.util.Comparator;
import javax.security.auth.Subject;

/**
 * Regression: a lambda class spun for JDK code is a member of the JDK module
 * that defined it, not of the unnamed module.
 *
 * {@code sun.security.util.ResourcesMgr} loads its bundle through the method
 * reference {@code ResourceBundle::getBundle}; {@code getBundle} judges the
 * lambda class as its caller, and a caller in the unnamed module may not load
 * {@code sun.security.util.resources.security} from {@code java.base}. CratonVM
 * answered the unnamed module for every VM-made lambda proxy, so the first
 * message {@code Subject} formatted threw {@code MissingResourceException}
 * (Tomcat {@code TestJaspicCallbackHandlerInAuthenticator}).
 */
public class RLambdaModule {
    public static void main(String[] args) {
        Comparator<String> jdkLambda = Comparator.comparing(String::length);
        System.out.println("CK jdkLambdaModule=" + jdkLambda.getClass().getModule());
        Runnable appLambda = () -> { };
        System.out.println("CK appLambdaNamed=" + appLambda.getClass().getModule().isNamed());

        Subject subject = new Subject();
        subject.setReadOnly();
        try {
            subject.getPrivateCredentials().add("x");
            System.out.println("CK readOnly=none");
        } catch (IllegalStateException e) {
            System.out.println("CK readOnly=" + e.getMessage());
        }
        System.out.println("PASS RLambdaModule");
    }
}
