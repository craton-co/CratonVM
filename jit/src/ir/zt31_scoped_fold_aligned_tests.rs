use super::{try_ir_scoped_unbox, UnboxOp};

const SMA: &str = "jdk/internal/misc/ScopedMemoryAccess";
const HEAD: &str = "(Ljdk/internal/foreign/MemorySessionImpl;Ljava/lang/Object;J";

fn fold(method: &str, tail: &str) -> Option<UnboxOp> {
    try_ir_scoped_unbox(SMA, method, &format!("{HEAD}{tail}"))
}

