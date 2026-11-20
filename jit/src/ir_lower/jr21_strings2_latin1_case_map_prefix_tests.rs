use super::*;
use crate::ir::IrBuilder;
use crate::ir_schedule;

fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the test wires only the slots it exercises.
    unsafe { std::mem::zeroed() }
}

const DESC: &str = "(Ljava/lang/String;[BLjava/util/Locale;)Ljava/lang/String;";


/// A compiled body held to the layout replacement epoch it baked: see the
/// comment in the test below.
struct EpochPinned<'a> {
    compile: Box<dyn Fn() -> CompiledMethod + 'a>,
    body: std::cell::RefCell<(CompiledMethod, u32)>,
}

impl<'a> EpochPinned<'a> {
    const TRIES: usize = 64;

    fn new(compile: impl Fn() -> CompiledMethod + 'a) -> Self {
        let compile: Box<dyn Fn() -> CompiledMethod + 'a> = Box::new(compile);
        let body = std::cell::RefCell::new(Self::compile_at_one_epoch(&*compile));
        Self { compile, body }
    }

    fn compile_at_one_epoch(compile: &dyn Fn() -> CompiledMethod) -> (CompiledMethod, u32) {
        for _ in 0..Self::TRIES {
            let epoch = cratonvm_types::layout_replace_epoch();
            let cm = compile();
            if cratonvm_types::layout_replace_epoch() == epoch {
                return (cm, epoch);
            }
        }
        panic!("the layout epoch kept moving under the compile");
    }

    /// # Safety
    ///
    /// `CompiledMethod::try_call_with_context`'s, with a null context, for
    /// every recompilation of the body.
    unsafe fn call(&self, args: &[i64]) -> i64 {
        let mut body = self.body.borrow_mut();
        for _ in 0..Self::TRIES {
            if cratonvm_types::layout_replace_epoch() != body.1 {
                *body = Self::compile_at_one_epoch(&*self.compile);
            }
            // SAFETY: this function's own contract.
            let r = unsafe { body.0.try_call_with_context(0, args) }.expect("call");
            if cratonvm_types::layout_replace_epoch() == body.1 {
                return r;
            }
        }
        panic!("the layout epoch kept moving under the test");
    }
}

