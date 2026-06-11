// SPB.8 retest probe: JIT'd Long.parseLong(String,int) historically crashed in
// the JIT prologue (an int local slot mapped onto the String parameter
// register — (String,int)->long calling convention). Hammer parseInt/parseLong
// past every threshold and compare an aggregate checksum with HotSpot.
public final class ParseProbe {
    public static void main(String[] args) {
        int reps = args.length > 0 ? Integer.parseInt(args[0]) : 200000;
        long sum = 0;
        for (int r = 0; r < reps; r++) {
            int k = r & 1023;
            sum += Integer.parseInt(Integer.toString(k));
            sum += Integer.parseInt(Integer.toString(k, 16), 16);
            sum += Long.parseLong(Long.toString(123456789000L + k));
            sum += Long.parseLong(Long.toString(0x123456789AL + k, 16), 16);
        }
        System.out.println("sum=" + sum);
    }
}
