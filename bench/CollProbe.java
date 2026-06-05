import java.util.*;
import java.util.concurrent.*;
import org.junit.runner.Description;

public class CollProbe {
    public static void main(String[] args) throws Exception {
        // 1) ConcurrentLinkedQueue basics
        ConcurrentLinkedQueue<String> q = new ConcurrentLinkedQueue<>();
        q.add("a"); q.add("b"); q.add("c"); q.add("d");
        System.out.println("CLQ.size()=" + q.size());
        System.out.println("CLQ.toArray().length=" + q.toArray().length);
        int it = 0; for (String s : q) it++;
        System.out.println("CLQ iterate count=" + it);
        ArrayList<String> al = new ArrayList<>(q);
        System.out.println("new ArrayList<>(CLQ).size()=" + al.size());
        int it2 = 0; for (String s : al) it2++;
        System.out.println("ArrayList iterate count=" + it2);

        // 2) Description manual build
        Description root = Description.createSuiteDescription("Root");
        for (int i = 0; i < 4; i++)
            root.addChild(Description.createTestDescription("Cls", "m" + i));
        System.out.println("Description.getChildren().size()=" + root.getChildren().size());
        System.out.println("Description.testCount()=" + root.testCount());
        int it3 = 0; for (Description d : root.getChildren()) it3++;
        System.out.println("Description iterate children=" + it3);
    }
}
