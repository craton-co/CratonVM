import org.junit.platform.engine.*;
import java.util.*;

public class EngineProbe {
    public static void main(String[] args) {
        ServiceLoader<TestEngine> sl = ServiceLoader.load(TestEngine.class);
        for (TestEngine e : sl) {
            System.out.println("ENGINE=" + e.getId());
        }
    }
}
