import java.util.List;
import java.util.ArrayList;
import java.util.stream.Collectors;

public class StreamCollect {
    public static void main(String[] args) {
        List<Integer> list = new ArrayList<>();
        for (int i = 0; i < 10; i++) list.add(i);

        System.out.println("Before collect: size=" + list.size());

        // This was causing stack overflow
        List<Integer> result = list.stream()
            .filter(x -> x > 3)
            .collect(Collectors.toList());

        System.out.println("After collect: size=" + result.size());
        System.out.println("Values: " + result);
    }
}
