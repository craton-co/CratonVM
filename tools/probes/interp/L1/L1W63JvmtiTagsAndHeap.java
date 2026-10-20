// The C JVMTI object tags and heap walks the table still answered with
// JVMTI_ERROR_NOT_AVAILABLE (98) after wave 61, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-lacks-the-functions-and-events-agents-call-first-FIXED-20261007.md,
// "Next step (from wave 61)", item 4): `GetTag` / `SetTag` (106 / 107),
// `GetObjectsWithTags` (114), `ForceGarbageCollection` (108), the heap
// walks `IterateThroughHeap` (116) and `FollowReferences` (115), and the
// JVMTI 1.0 walks (109-112).
//
// The probe builds a small graph of `Node`s the walks are asked about (by
// class, by tag, from an initial object), so every row is about objects it
// made; the sizes are HotSpot's (compressed oops).
//
// Needs the native shim tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w63heap.so tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.c
//   javac -d /tmp/l1w63heap tools/probes/interp/L1/L1W63JvmtiTagsAndHeap.java
//   java     -agentpath:/tmp/libl1w63heap.so -cp /tmp/l1w63heap L1W63JvmtiTagsAndHeap /tmp/libl1w63heap.so
//   cratonvm -agentpath:/tmp/libl1w63heap.so -cp /tmp/l1w63heap L1W63JvmtiTagsAndHeap /tmp/libl1w63heap.so
//
// The Rust port of the shim is
// tools/probes/interp/L1/rust-ports/L1W63JvmtiTagsAndHeap.
//
// Expected stdout (HotSpot 25.0.3): see the block at the end of this file.
public class L1W63JvmtiTagsAndHeap {
    public static class Node {
        public Node left;
        public Node right;
        public int value;
        public Object[] more;

        public Node(int value) {
            this.value = value;
        }
    }

    public static class Leaf extends Node {
        public long weight;

        public Leaf(int value) {
            super(value);
        }
    }

    /** `GetTag(o)`, or `-err`. */
    static native long getTag(Object o);

    /** `SetTag(o, tag)`'s error. */
    static native int setTag(Object o, long tag);

    /** The argument-error rows of the tag functions. */
    static native String tagErrors(Object any);

    /** `GetObjectsWithTags(tags)`: `err count` then each `tag` sorted. */
    static native String objectsWithTags(long[] tags);

    /** `ForceGarbageCollection`'s error. */
    static native int forceGc();

    /** `IterateThroughHeap(filter, klass)` with a counting callback that tags
     *  each object it sees with `retag` (0: leaves the tag), aborting after
     *  `abortAfter` objects (0: never). */
    static native String throughHeap(int filter, Class<?> klass, long retag, int abortAfter);

    /** `IterateThroughHeap` over `klass` with the array, string and field
     *  callbacks; the rows are about the objects tagged `tag`. */
    static native String primitives(Class<?> klass, long tag);

    /** `FollowReferences(0, klass, initial)` with a callback that reports
     *  every reference to a tagged object, and follows only tagged ones. */
    static native String follow(Class<?> klass, Object initial, int abortAfter);

    /** `FollowReferences` from the roots, counting tagged objects reached. */
    static native String followRoots(Class<?> klass);

    /** The JVMTI 1.0 walks: `IterateOverHeap`, `IterateOverInstancesOfClass`,
     *  `IterateOverObjectsReachableFromObject`, `IterateOverReachableObjects`. */
    static native String oldWalks(Class<?> klass, Object initial);

    /** The argument-error rows of the walks. */
    static native String walkErrors(Class<?> klass, Object any);

    static Node graph;
    static int[] numbers = {1, 2, 3, 4, 5};
    static String text = new String(new char[] {'t', 'a', 'g', 'g', 'e', 'd'});

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W63JvmtiTagsAndHeap <path of the shim library>");
            return;
        }
        System.load(args[0]);
        Object plain = new Object();
        System.out.println("tag untagged: " + getTag(plain));
        System.out.println("set 42: err=" + setTag(plain, 42));
        System.out.println("tag after set: " + getTag(plain));
        System.out.println("set 0: err=" + setTag(plain, 0));
        System.out.println("tag after untag: " + getTag(plain));
        System.out.println("tag of String.class: " + getTag(String.class));
        System.out.print(tagErrors(plain));

        Node root = new Node(1);
        root.left = new Node(2);
        root.right = new Leaf(3);
        root.left.left = root.right;
        root.more = new Object[] {new Node(4), "s", null, new Leaf(5)};
        graph = root;
        Node[] all = {root, root.left, (Node) root.right, (Node) root.more[0], (Node) root.more[3]};
        for (int i = 0; i < all.length; i++) {
            setTag(all[i], 10 + i);
        }
        setTag(root.more, 15);
        Node stray = new Node(99);
        System.out.println("with tags 10,12,14: " + objectsWithTags(new long[] {10, 12, 14}));
        System.out.println("with tag 77: " + objectsWithTags(new long[] {77}));

        System.out.println("heap Node all: " + throughHeap(0, Node.class, 0, 0));
        System.out.println("heap Leaf all: " + throughHeap(0, Leaf.class, 0, 0));
        System.out.println("heap Node tagged: " + throughHeap(4, Node.class, 0, 0));
        System.out.println("heap Node untagged: " + throughHeap(8, Node.class, 0, 0));
        setTag(Leaf.class, 500);
        System.out.println("heap Node class-tagged: " + throughHeap(16, Node.class, 0, 0));
        System.out.println("heap Node class-untagged: " + throughHeap(32, Node.class, 0, 0));
        System.out.println("heap Node abort: " + throughHeap(0, Node.class, 0, 1));
        System.out.println("heap Node retag untagged 600: " + throughHeap(4, Node.class, 600, 0));
        System.out.println("with tag 600: " + objectsWithTags(new long[] {600}));
        setTag(stray, 0);
        setTag(numbers, 700);
        setTag(text, 701);
        System.out.println("primitives int[]: " + primitives(int[].class, 700));
        System.out.println("primitives String: " + primitives(String.class, 701));
        System.out.println("primitives Node: " + primitives(Node.class, 10));

        System.out.print(follow(Node.class, root, 0));
        System.out.print(follow(Leaf.class, root, 0));
        System.out.print(follow(null, root, 2));
        System.out.print(followRoots(Node.class));
        System.out.print(oldWalks(Node.class, root));
        System.out.print(walkErrors(Node.class, root));

        Object doomed = new Node(1000);
        setTag(doomed, 900);
        doomed = null;
        System.out.println("gc: err=" + forceGc());
        System.out.println("with tag 900 after gc: " + objectsWithTags(new long[] {900}));
        System.out.println("stray kept: " + (stray.value == 99) + " graph kept: " + (graph.value == 1));
        System.out.println("done");
    }
}

// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   tag untagged: 0
//   set 42: err=0
//   tag after set: 42
//   set 0: err=0
//   tag after untag: 0
//   tag of String.class: 0
//   GetTag null object: err=20
//   GetTag null tag_ptr: err=100
//   SetTag null object: err=20
//   GetObjectsWithTags negative: err=103
//   GetObjectsWithTags null tags: err=100
//   GetObjectsWithTags tag 0: err=0
//   GetObjectsWithTags null count: err=100
//   with tags 10,12,14: err=0 count=3 objects=1 tags=[10,12,14]
//   with tag 77: err=0 count=0 objects=1 tags=[]
//   heap Node all: err=0 count=4 tagged=3 sizes=[32] lengths=[-1] class-tags=[0]
//   heap Leaf all: err=0 count=2 tagged=2 sizes=[40] lengths=[-1] class-tags=[0]
//   heap Node tagged: err=0 count=1 tagged=0 sizes=[32] lengths=[-1] class-tags=[0]
//   heap Node untagged: err=0 count=3 tagged=3 sizes=[32] lengths=[-1] class-tags=[0]
//   heap Node class-tagged: err=0 count=4 tagged=3 sizes=[32] lengths=[-1] class-tags=[0]
//   heap Node class-untagged: err=0 count=0 tagged=0 sizes=[] lengths=[] class-tags=[]
//   heap Node abort: err=0 count=1 tagged=1 sizes=[32] lengths=[-1] class-tags=[0]
//   heap Node retag untagged 600: err=0 count=1 tagged=0 sizes=[32] lengths=[-1] class-tags=[0]
//   with tag 600: err=0 count=1 objects=1 tags=[600]
//   primitives int[]: err=0 [array size=40 count=5 type=73 sum=15]
//   primitives String: err=0 [field kind=2 index=1 type=66 value=0; field kind=2 index=2 type=73 value=0; field kind=2 index=3 type=90 value=false; string size=24 length=6 value=tagged]
//   primitives Node: err=0 [field kind=2 index=2 type=73 value=1]
//   follow klass=set abort=0: err=0 calls-to-tagged=3
//     kind=2 10->11 index=0 size=32 length=-1
//     kind=3 15->13 index=0 size=32 length=-1
//     kind=8 0->10 index=0 size=32 length=-1
//   follow klass=set abort=0: err=0 calls-to-tagged=3
//     kind=2 10->12 index=1 size=40 length=-1
//     kind=2 11->12 index=0 size=40 length=-1
//     kind=3 15->14 index=3 size=40 length=-1
//   follow abort=2: err=0 calls=2
//   follow roots: err=0 tags-reached=[10,11,13] kinds-to-root-node=[3,8,24]
//   IterateOverHeap tagged: err=0 count-at-least-7=1
//   IterateOverInstancesOfClass Node: err=0 count=6 tagged=5 sizes=[32,40]
//   IterateOverObjectsReachableFromObject: err=0
//     kind=1 12->500 index=-1
//     kind=1 14->500 index=-1
//     kind=2 10->11 index=0
//     kind=2 10->12 index=1
//     kind=2 10->15 index=3
//     kind=2 11->12 index=0
//     kind=3 15->13 index=0
//     kind=3 15->14 index=3
//     kind=9 500->500 index=7
//   IterateOverReachableObjects: err=0 tags-reached=[10,11,12,13,14,15]
//   IterateThroughHeap null callbacks: err=100
//   IterateThroughHeap object as klass: err=0
//   FollowReferences null callbacks: err=100
//   FollowReferences object as klass: err=0
//   IterateOverHeap null callback: err=100
//   IterateOverHeap filter 9: err=0
//   IterateOverInstancesOfClass null klass: err=21
//   IterateOverObjectsReachableFromObject null object: err=20
//   gc: err=0
//   with tag 900 after gc: err=0 count=0 objects=1 tags=[]
//   stray kept: true graph kept: true
//   done
