import java.util.*;
import java.util.concurrent.*;

public class CollProbe2 {
    public static void main(String[] args) throws Exception {
        ConcurrentLinkedQueue<String> q = new ConcurrentLinkedQueue<>();
        q.add("a"); q.add("b"); q.add("c"); q.add("d");
        System.out.println("clq.getClass().getName()=" + q.getClass().getName());
        System.out.println("clq.size()=" + q.size());

        // Path 1: constructor
        ArrayList<String> viaCtor = new ArrayList<>(q);
        System.out.println("new ArrayList<>(clq).size()=" + viaCtor.size());

        // Path 2: addAll
        ArrayList<String> viaAddAll = new ArrayList<>();
        boolean ch = viaAddAll.addAll(q);
        System.out.println("arrayList.addAll(clq) changed=" + ch + " size=" + viaAddAll.size());

        // Path 3: HashSet ctor
        HashSet<String> viaHashSet = new HashSet<>(q);
        System.out.println("new HashSet<>(clq).size()=" + viaHashSet.size());

        // Controls
        LinkedList<String> ll = new LinkedList<>();
        ll.add("x"); ll.add("y");
        System.out.println("new ArrayList<>(linkedList[2]).size()=" + new ArrayList<>(ll).size());
        System.out.println("new ArrayList<>(List.of(1,2,3)).size()=" + new ArrayList<>(List.of(1,2,3)).size());

        // LinkedBlockingQueue (also LBQ-backed)
        LinkedBlockingQueue<String> lbq = new LinkedBlockingQueue<>();
        lbq.add("p"); lbq.add("r");
        System.out.println("lbq.getClass()=" + lbq.getClass().getName() + " lbq.size()=" + lbq.size());
        System.out.println("new ArrayList<>(lbq[2]).size()=" + new ArrayList<>(lbq).size());
    }
}
