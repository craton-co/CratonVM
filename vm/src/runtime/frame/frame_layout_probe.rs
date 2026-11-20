use super::*;

/// Field-by-field accounting of the 296-byte `Frame`, which is built and
/// then moved by value into the frame stack on EVERY interpreted call.
#[test]
fn report_frame_layout() {
    eprintln!("Frame                 = {}", std::mem::size_of::<Frame>());
    eprintln!(
        "  FrameInner          = {}",
        std::mem::size_of::<FrameInner>()
    );
    eprintln!(
        "  ValueStack          = {}",
        std::mem::size_of::<ValueStack>()
    );
    eprintln!(
        "  Vec<CompactValue>   = {}",
        std::mem::size_of::<Vec<CompactValue>>()
    );
    eprintln!("  Vec<u8>             = {}", std::mem::size_of::<Vec<u8>>());
    eprintln!(
        "  Vec<(usize,u32)>    = {}",
        std::mem::size_of::<Vec<(usize, u32)>>()
    );
    eprintln!(
        "  Arc<[u8]>           = {}",
        std::mem::size_of::<Arc<[u8]>>()
    );
    eprintln!(
        "  Option<ObjectRef>   = {}",
        std::mem::size_of::<Option<ObjectRef>>()
    );
    eprintln!("--- FrameInner variants ---");
    eprintln!(
        "  Arc<CachedBytecodeMethod> (Cached payload) = {}",
        std::mem::size_of::<Arc<CachedBytecodeMethod>>()
    );
    eprintln!(
        "  Owned payload (5 fat ptrs)                 = {}",
        std::mem::size_of::<Arc<str>>() * 3
            + std::mem::size_of::<Option<Arc<str>>>()
            + std::mem::size_of::<Arc<[ExceptionTableEntry]>>()
    );
}
