import java.util.regex.*; import java.util.*; import java.util.stream.*;
public class P5Regex { public static void main(String[] a) {
  Matcher m = Pattern.compile("(\\w+)@(\\w+)\\.com").matcher("mail bob@ex.com and al@zz.com");
  while (m.find()) System.out.println(m.group(1) + "|" + m.group(2));
  System.out.println("a1b22c333".replaceAll("\\d+", "#") + " " + Arrays.toString("x,y,,z".split(",")));
  System.out.println(IntStream.rangeClosed(1, 10).filter(i -> i % 2 == 0).boxed().collect(Collectors.toList()));
  System.out.println(Stream.of("b","a","c").sorted().collect(Collectors.joining("-")) + " " + IntStream.range(0,100).parallel().sum());
  Map<Boolean, Long> p = IntStream.range(0, 20).boxed().collect(Collectors.partitioningBy(i -> i > 5, Collectors.counting())); System.out.println(p);
  System.out.println(new Scanner("12 abc 3.5").nextInt());
}}
