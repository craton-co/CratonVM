import org.junit.runner.JUnitCore;
import org.junit.runner.Result;
import org.junit.runner.notification.Failure;
public final class NumericalJUnitProbe {
    public static void main(String[] args) throws Exception {
        Result r=JUnitCore.runClasses(Class.forName(args[0]));
        System.out.println(args[0]+" tests="+r.getRunCount()+" ignored="+r.getIgnoreCount()+" failures="+r.getFailureCount());
        for(Failure f:r.getFailures()) System.out.println(f.getTestHeader()+" "+f.getException().getClass().getName()+" "+f.getMessage());
        if(!r.wasSuccessful()) System.exit(1);
    }
}
