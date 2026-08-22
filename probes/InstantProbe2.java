import java.time.*;

/** Decomposes Instant.now() into its JDK-internal steps to locate the 150x. */
public class InstantProbe2 {
    static long sink; static Object osink;

    static void bare(int n){ for(int i=0;i<n;i++) sink+=i; }
    static void currentMillis(int n){ for(int i=0;i<n;i++) sink+=System.currentTimeMillis(); }
    static void nanoTime(int n){ for(int i=0;i<n;i++) sink+=System.nanoTime(); }
    static void ofEpochSecondJJ(int n){ for(int i=0;i<n;i++) osink=Instant.ofEpochSecond(1700000000L, 123456789L); }
    static void ofEpochSecondJ(int n){ for(int i=0;i<n;i++) osink=Instant.ofEpochSecond(1700000000L); }
    static void ofEpochMilli(int n){ for(int i=0;i<n;i++) osink=Instant.ofEpochMilli(1700000000123L); }
    static void clockSystemUTC(int n){ for(int i=0;i<n;i++) osink=Clock.systemUTC(); }
    static void clockInstant(int n){ Clock c=Clock.systemUTC(); for(int i=0;i<n;i++) osink=c.instant(); }
    static void clockMillis(int n){ Clock c=Clock.systemUTC(); for(int i=0;i<n;i++) sink+=c.millis(); }
    static void instantNow(int n){ for(int i=0;i<n;i++) osink=Instant.now(); }
    static void instantGetNano(int n){ Instant a=Instant.ofEpochSecond(1,2); for(int i=0;i<n;i++) sink+=a.getNano(); }
    static void instantToEpochMilli(int n){ Instant a=Instant.ofEpochSecond(1,2); for(int i=0;i<n;i++) sink+=a.toEpochMilli(); }
    static void instantPlusMillis(int n){ Instant a=Instant.ofEpochSecond(1,2); for(int i=0;i<n;i++) osink=a.plusMillis(3); }
    static void instantIsBefore(int n){ Instant a=Instant.ofEpochSecond(1,2), b=Instant.ofEpochSecond(9,2);
        for(int i=0;i<n;i++) sink+=a.isBefore(b)?1:0; }
    static void durationBetween(int n){ Instant a=Instant.ofEpochSecond(1,2), b=Instant.ofEpochSecond(9,2);
        for(int i=0;i<n;i++) osink=Duration.between(a,b); }
    static void durationOfMillis(int n){ for(int i=0;i<n;i++) osink=Duration.ofMillis(5); }
    static void mathFloorDiv(int n){ for(int i=0;i<n;i++) sink+=Math.floorDiv((long)i, 1000L); }
    static void mathAddExact(int n){ for(int i=0;i<n;i++) sink+=Math.addExact((long)i, 7L); }
    static void newObject(int n){ for(int i=0;i<n;i++) osink=new Object(); }
    static void localDateTimeNow(int n){ for(int i=0;i<n;i++) osink=LocalDateTime.now(); }

    interface L { void run(int n); }
    public static void main(String[] a){
        int n = a.length>0?Integer.parseInt(a[0]):100000;
        String[] names={"bare","System.currentTimeMillis","System.nanoTime","new Object",
            "Math.floorDiv","Math.addExact","Instant.ofEpochSecond(JJ)","Instant.ofEpochSecond(J)",
            "Instant.ofEpochMilli","Clock.systemUTC()","Clock.instant()","Clock.millis()",
            "Instant.now()","Instant.getNano","Instant.toEpochMilli","Instant.plusMillis",
            "Instant.isBefore","Duration.between","Duration.ofMillis","LocalDateTime.now()"};
        L[] fns={InstantProbe2::bare,InstantProbe2::currentMillis,InstantProbe2::nanoTime,InstantProbe2::newObject,
            InstantProbe2::mathFloorDiv,InstantProbe2::mathAddExact,InstantProbe2::ofEpochSecondJJ,InstantProbe2::ofEpochSecondJ,
            InstantProbe2::ofEpochMilli,InstantProbe2::clockSystemUTC,InstantProbe2::clockInstant,InstantProbe2::clockMillis,
            InstantProbe2::instantNow,InstantProbe2::instantGetNano,InstantProbe2::instantToEpochMilli,InstantProbe2::instantPlusMillis,
            InstantProbe2::instantIsBefore,InstantProbe2::durationBetween,InstantProbe2::durationOfMillis,InstantProbe2::localDateTimeNow};
        for(int pass=0;pass<2;pass++){
            System.out.println("--- pass "+pass+" ---");
            for(int k=0;k<names.length;k++){
                fns[k].run(Math.min(n,10000));
                long t0=System.nanoTime(); fns[k].run(n); long d=System.nanoTime()-t0;
                System.out.printf("%-28s %10.1f ns/op%n",names[k],(double)d/n);
                System.out.flush();
            }
        }
        System.out.println("sink="+sink+" "+(osink!=null));
        Runtime.getRuntime().halt(0);
    }
}
