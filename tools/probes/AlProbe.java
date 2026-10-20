import java.util.ArrayList;
import java.util.List;

public class AlProbe {
    static ArrayList<Integer> al = new ArrayList<>();

    static int sumList(List<Integer> l) {
        int s = 0;
        int n = l.size();
        for (int i = 0; i < n; i++) s += l.get(i);
        return s;
    }

    static int sumAl(ArrayList<Integer> l) {
        int s = 0;
        for (int i = 0; i < l.size(); i++) s += l.get(i);
        return s;
    }

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "get";
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 400_000;
        int rounds = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        for (int i = 0; i < 16; i++) al.add(i);
        long chk = 0;
        for (int r = 0; r <= rounds; r++) {
            long t0 = System.nanoTime();
            long s = 0;
            if (mode.equals("get")) { for (int i = 0; i < n; i++) s += sumAl(al); }
            else { for (int i = 0; i < n; i++) s += sumList(al); }
            chk = s;
            long ms = (System.nanoTime() - t0) / 1_000_000;
            System.out.println("r" + r + " " + mode + " " + ms + " ms chk=" + chk);
        }
    }
}
