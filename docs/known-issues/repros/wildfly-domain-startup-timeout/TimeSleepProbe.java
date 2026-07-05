class TimeSleepProbe {
    public static void main(String[] args) throws Exception {
        long millisStart = System.currentTimeMillis();
        long nanosStart = System.nanoTime();
        Thread.sleep(2000);
        long millisEnd = System.currentTimeMillis();
        long nanosEnd = System.nanoTime();
        System.out.println("millis=" + (millisEnd - millisStart)
                + " nanosMs=" + ((nanosEnd - nanosStart) / 1_000_000));
    }
}
