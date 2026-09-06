import java.lang.instrument.Instrumentation;

public class RedefAgent {
    public static volatile Instrumentation inst;
    public static void premain(String args, Instrumentation i) { inst = i; }
}
