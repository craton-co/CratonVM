//! Verification frame — the state tracked during bytecode verification.
//!
//! A `VerificationFrame` represents the types in local variables and on the
//! operand stack at a specific bytecode offset. The verifier maintains a
//! current frame and advances it through each instruction.

use rustjvm_reader::constant_pool::ConstantPool;
use rustjvm_reader::stack_map::StackMapFrame;

use super::vtype::{param_types_from_descriptor, ClassHierarchy, VType};
use rustjvm_types::error::LinkageError;

/// The verification frame: local variable types and operand stack types.
#[derive(Debug, Clone)]
pub struct VerificationFrame {
    /// Local variable types. May contain `Top` for undefined/unusable slots.
    pub locals: Vec<VType>,
    /// Operand stack types, bottom-to-top.
    pub stack: Vec<VType>,
    /// Maximum stack size (from Code attribute).
    max_stack: u16,
}

impl VerificationFrame {
    /// Build the initial frame for a method.
    ///
    /// Per JVM spec 4.10.1.6:
    /// - Instance methods: local[0] = `ObjectRef(class)` (or `UninitializedThis` for `<init>`)
    /// - Static methods: no implicit `this`
    /// - Parameters fill subsequent locals
    /// - Category-2 types occupy two slots (the type + Top)
    /// - Stack is empty
    pub fn initial_frame(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        is_static: bool,
        max_locals: u16,
        max_stack: u16,
    ) -> Self {
        let mut locals = Vec::with_capacity(max_locals as usize);

        // Instance methods get `this` in local[0]
        if !is_static {
            if method_name == "<init>" {
                locals.push(VType::UninitializedThis);
            } else {
                locals.push(VType::ObjectRef(std::sync::Arc::from(class_name)));
            }
        }

        // Parse parameter types from descriptor and add to locals
        for param_type in param_types_from_descriptor(descriptor) {
            let is_cat2 = param_type.is_category2();
            locals.push(param_type);
            if is_cat2 {
                locals.push(VType::Top); // second slot for long/double
            }
        }

        // Fill remaining locals with Top
        while locals.len() < max_locals as usize {
            locals.push(VType::Top);
        }

        VerificationFrame {
            locals,
            stack: Vec::new(),
            max_stack,
        }
    }

    /// Build a compact initial frame for StackMapTable derivation.
    ///
    /// Per JVM spec 4.7.4, the initial frame used to derive StackMapTable entries
    /// contains ONLY the method parameters (not padded to max_locals). AppendFrame
    /// and ChopFrame operations modify this compact frame.
    pub fn compact_initial_frame(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        is_static: bool,
        max_stack: u16,
    ) -> Self {
        let mut locals = Vec::new();

        // Instance methods get `this` in local[0]
        if !is_static {
            if method_name == "<init>" {
                locals.push(VType::UninitializedThis);
            } else {
                locals.push(VType::ObjectRef(std::sync::Arc::from(class_name)));
            }
        }

        // Parse parameter types from descriptor and add to locals
        for param_type in param_types_from_descriptor(descriptor) {
            let is_cat2 = param_type.is_category2();
            locals.push(param_type);
            if is_cat2 {
                locals.push(VType::Top); // second slot for long/double
            }
        }

        // Do NOT pad to max_locals — StackMapTable frames are derived from this compact form
        VerificationFrame {
            locals,
            stack: Vec::new(),
            max_stack,
        }
    }

    /// Apply a StackMapTable frame to produce a new verification frame.
    ///
    /// Per JVM spec 4.7.4, each frame type modifies the previous frame differently.
    pub fn apply_stack_map_frame(
        &self,
        frame: &StackMapFrame,
        cp: &ConstantPool,
    ) -> Result<Self, LinkageError> {
        match frame {
            StackMapFrame::SameFrame { .. } => {
                // Same locals, empty stack
                Ok(VerificationFrame {
                    locals: self.locals.clone(),
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                })
            }

            StackMapFrame::SameLocals1StackItem { stack, .. }
            | StackMapFrame::SameLocals1StackItemExtended { stack, .. } => {
                let vtype = VType::from_verification_type_info(stack, cp)?;
                let is_cat2 = vtype.is_category2();
                let mut new_stack = Vec::with_capacity(if is_cat2 { 2 } else { 1 });
                new_stack.push(vtype);
                // Long and Double occupy two stack slots (value + Top)
                if is_cat2 {
                    new_stack.push(VType::Top);
                }
                Ok(VerificationFrame {
                    locals: self.locals.clone(),
                    stack: new_stack,
                    max_stack: self.max_stack,
                })
            }

            StackMapFrame::ChopFrame { chopped, .. } => {
                let mut locals = self.locals.clone();
                // Remove the last `chopped` logical locals.
                // Category-2 types (Long/Double) occupy two slots (value + Top),
                // so chopping one logical local may require popping 2 entries.
                for _ in 0..*chopped {
                    match locals.pop() {
                        None => {
                            return Err(verify_error("chop_frame: not enough locals to chop"));
                        }
                        Some(VType::Top) => {
                            // This Top may be the companion slot of a Long/Double.
                            // If so, also pop the Long/Double itself.
                            if let Some(prev) = locals.last() {
                                if prev.is_category2() {
                                    locals.pop();
                                }
                            }
                        }
                        Some(_) => {
                            // Single-slot type — already removed
                        }
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                })
            }

            StackMapFrame::SameFrameExtended { .. } => Ok(VerificationFrame {
                locals: self.locals.clone(),
                stack: Vec::new(),
                max_stack: self.max_stack,
            }),

            StackMapFrame::AppendFrame {
                locals: new_locals, ..
            } => {
                let mut locals = self.locals.clone();
                for info in new_locals {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    locals.push(vtype);
                    // Long and Double occupy two local slots (value + Top)
                    if is_cat2 {
                        locals.push(VType::Top);
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack: Vec::new(),
                    max_stack: self.max_stack,
                })
            }

            StackMapFrame::FullFrame {
                locals: frame_locals,
                stack: frame_stack,
                ..
            } => {
                let mut locals = Vec::with_capacity(frame_locals.len());
                for info in frame_locals {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    locals.push(vtype);
                    // Long and Double occupy two local slots (value + Top)
                    if is_cat2 {
                        locals.push(VType::Top);
                    }
                }
                let mut stack = Vec::with_capacity(frame_stack.len());
                for info in frame_stack {
                    let vtype = VType::from_verification_type_info(info, cp)?;
                    let is_cat2 = vtype.is_category2();
                    stack.push(vtype);
                    // Long and Double occupy two stack slots (value + Top)
                    if is_cat2 {
                        stack.push(VType::Top);
                    }
                }
                Ok(VerificationFrame {
                    locals,
                    stack,
                    max_stack: self.max_stack,
                })
            }
        }
    }

    /// Check if this frame is assignable to `target` (all slots assignable).
    pub fn is_assignable_to(
        &self,
        target: &VerificationFrame,
        hierarchy: &dyn ClassHierarchy,
    ) -> bool {
        // Stack must be the same length
        if self.stack.len() != target.stack.len() {
            return false;
        }

        // Each stack entry must be assignable
        for (a, b) in self.stack.iter().zip(target.stack.iter()) {
            if !a.is_assignable_to(b, hierarchy) {
                return false;
            }
        }

        // Each local must be assignable (up to target's length)
        // If target has more locals, they must all be Top in the source
        for (i, target_local) in target.locals.iter().enumerate() {
            let source_local = self.locals.get(i).unwrap_or(&VType::Top);
            if !source_local.is_assignable_to(target_local, hierarchy) {
                return false;
            }
        }

        true
    }

    /// Merge this frame with another, producing the least upper bound.
    pub fn merge(
        &self,
        other: &VerificationFrame,
        hierarchy: &dyn ClassHierarchy,
    ) -> Result<VerificationFrame, LinkageError> {
        if self.stack.len() != other.stack.len() {
            return Err(verify_error(
                "cannot merge frames with different stack sizes",
            ));
        }

        let stack: Vec<VType> = self
            .stack
            .iter()
            .zip(other.stack.iter())
            .map(|(a, b)| a.merge(b, hierarchy))
            .collect();

        let max_len = self.locals.len().max(other.locals.len());
        let mut locals = Vec::with_capacity(max_len);
        for i in 0..max_len {
            let a = self.locals.get(i).unwrap_or(&VType::Top);
            let b = other.locals.get(i).unwrap_or(&VType::Top);
            locals.push(a.merge(b, hierarchy));
        }

        Ok(VerificationFrame {
            locals,
            stack,
            max_stack: self.max_stack,
        })
    }

    // ----- Stack operations -------------------------------------------------

    /// Push a type onto the operand stack.
    pub fn push(&mut self, vtype: VType) -> Result<(), LinkageError> {
        if self.stack.len() >= self.max_stack as usize {
            return Err(verify_error("stack overflow during verification"));
        }
        self.stack.push(vtype);
        Ok(())
    }

    /// Pop a type from the operand stack.
    pub fn pop(&mut self) -> Result<VType, LinkageError> {
        self.stack
            .pop()
            .ok_or_else(|| verify_error("stack underflow during verification"))
    }

    /// Pop a type and check that it's assignable to `expected`.
    pub fn pop_expect(
        &mut self,
        expected: &VType,
        hierarchy: &dyn ClassHierarchy,
    ) -> Result<VType, LinkageError> {
        let actual = self.pop()?;
        if !actual.is_assignable_to(expected, hierarchy) {
            return Err(verify_error(&format!(
                "expected {expected:?} on stack, found {actual:?}"
            )));
        }
        Ok(actual)
    }

    /// Clear the stack (used when entering exception handlers).
    pub fn clear_stack(&mut self) {
        self.stack.clear();
    }

    /// Pad locals to max_locals by filling with Top.
    /// StackMapTable-derived frames may have fewer locals than max_locals.
    pub fn pad_locals_to(&mut self, max_locals: u16) {
        while self.locals.len() < max_locals as usize {
            self.locals.push(VType::Top);
        }
    }

    // ----- Local variable operations ----------------------------------------

    /// Load a type from a local variable slot.
    pub fn local_load(&self, index: u16) -> Result<&VType, LinkageError> {
        self.locals.get(index as usize).ok_or_else(|| {
            verify_error(&format!(
                "local variable index {index} out of range (max {})",
                self.locals.len()
            ))
        })
    }

    /// Store a type into a local variable slot.
    pub fn local_store(&mut self, index: u16, vtype: VType) -> Result<(), LinkageError> {
        let idx = index as usize;
        if idx >= self.locals.len() {
            return Err(verify_error(&format!(
                "local variable index {index} out of range (max {})",
                self.locals.len()
            )));
        }
        self.locals[idx] = vtype;
        Ok(())
    }

    /// Get the current stack depth.
    pub fn stack_depth(&self) -> usize {
        self.stack.len()
    }
}

fn verify_error(message: &str) -> LinkageError {
    LinkageError::VerifyError {
        class_name: String::new(),
        method_name: String::new(),
        message: message.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vtype::VType;
    use std::sync::Arc;

    struct MockHierarchy;

    impl ClassHierarchy for MockHierarchy {
        fn is_subclass(&self, child: &str, parent: &str) -> bool {
            if child == parent || parent == "java/lang/Object" {
                return true;
            }
            // String implements CharSequence (used by frame-merge tests).
            matches!(
                (child, parent),
                ("java/lang/String", "java/lang/CharSequence")
            )
        }

        fn common_superclass(&self, _a: &str, _b: &str) -> String {
            "java/lang/Object".to_string()
        }

        fn is_interface(&self, _name: &str) -> bool {
            false
        }
    }

    #[test]
    fn initial_frame_static_void() {
        let frame = VerificationFrame::initial_frame(
            "com/example/Foo",
            "bar",
            "()V",
            true, // static
            1,
            2,
        );
        // Static method with no params: all locals are Top
        assert_eq!(frame.locals, vec![VType::Top]);
        assert!(frame.stack.is_empty());
    }

    #[test]
    fn initial_frame_instance_method() {
        let frame = VerificationFrame::initial_frame(
            "com/example/Foo",
            "bar",
            "(I)V",
            false, // instance
            3,
            2,
        );
        // local[0] = Foo, local[1] = Int, local[2] = Top
        assert_eq!(
            frame.locals[0],
            VType::ObjectRef(Arc::from("com/example/Foo"))
        );
        assert_eq!(frame.locals[1], VType::Int);
        assert_eq!(frame.locals[2], VType::Top);
    }

    #[test]
    fn initial_frame_constructor() {
        let frame =
            VerificationFrame::initial_frame("com/example/Foo", "<init>", "()V", false, 1, 1);
        assert_eq!(frame.locals[0], VType::UninitializedThis);
    }

    #[test]
    fn initial_frame_long_param() {
        let frame = VerificationFrame::initial_frame("Foo", "m", "(J)V", true, 3, 1);
        // local[0] = Long, local[1] = Top (second slot), local[2] = Top
        assert_eq!(frame.locals[0], VType::Long);
        assert_eq!(frame.locals[1], VType::Top);
        assert_eq!(frame.locals[2], VType::Top);
    }

    #[test]
    fn push_pop() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
        frame.push(VType::Int).unwrap();
        frame.push(VType::Float).unwrap();
        assert_eq!(frame.stack_depth(), 2);

        let top = frame.pop().unwrap();
        assert_eq!(top, VType::Float);
        assert_eq!(frame.stack_depth(), 1);
    }

    #[test]
    fn pop_empty_stack_errors() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.pop().is_err());
    }

    #[test]
    fn push_overflow_errors() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        frame.push(VType::Int).unwrap();
        assert!(frame.push(VType::Int).is_err());
    }

    #[test]
    fn pop_expect_matching() {
        let h = MockHierarchy;
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        frame.push(VType::Int).unwrap();
        assert!(frame.pop_expect(&VType::Int, &h).is_ok());
    }

    #[test]
    fn pop_expect_wrong_type() {
        let h = MockHierarchy;
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        frame.push(VType::Float).unwrap();
        assert!(frame.pop_expect(&VType::Int, &h).is_err());
    }

    #[test]
    fn local_load_store() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 3, 1);
        frame.local_store(1, VType::Int).unwrap();
        assert_eq!(frame.local_load(1).unwrap(), &VType::Int);
    }

    #[test]
    fn local_out_of_range_errors() {
        let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.local_load(5).is_err());
    }

    #[test]
    fn frame_is_assignable_to_self() {
        let h = MockHierarchy;
        let frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        assert!(frame.is_assignable_to(&frame, &h));
    }

    #[test]
    fn frame_merge_same_is_same() {
        let h = MockHierarchy;
        let frame = VerificationFrame::initial_frame("Foo", "m", "(I)V", true, 2, 2);
        let merged = frame.merge(&frame, &h).unwrap();
        assert_eq!(merged.locals, frame.locals);
    }

    #[test]
    fn frame_merge_different_stack_depth_errors() {
        let h = MockHierarchy;
        let mut frame1 = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 2);
        let frame2 = frame1.clone();
        frame1.push(VType::Int).unwrap();
        assert!(frame1.merge(&frame2, &h).is_err());
    }

    #[test]
    fn frame_assignable_string_to_charsequence_local() {
        // Regression: at a StackMapTable merge point, a local typed as
        // String in the current frame must be assignable to a declared
        // local of CharSequence (String implements CharSequence).
        let h = MockHierarchy;
        let mut current =
            VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        current.locals[0] = VType::ObjectRef(Arc::from("java/lang/String"));
        let mut declared =
            VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 1);
        declared.locals[0] = VType::ObjectRef(Arc::from("java/lang/CharSequence"));
        assert!(current.is_assignable_to(&declared, &h));
    }

    #[test]
    fn frame_assignable_int_local_to_top_declared() {
        // Regression (B2): a StackMapTable full_frame may declare a local
        // slot as Top to indicate the value is unused past the merge point.
        // The current frame may still carry a concrete type (e.g. Int) from
        // a fall-through path; it must still be considered assignable since
        // Top is the top of the verification type lattice (JVMS 4.10.1.2).
        let h = MockHierarchy;
        let mut current =
            VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        current.locals[3] = VType::Int;
        let mut declared =
            VerificationFrame::initial_frame("Foo", "m", "()V", true, 4, 1);
        declared.locals[3] = VType::Top;
        assert!(current.is_assignable_to(&declared, &h));
    }

    #[test]
    fn clear_stack() {
        let mut frame = VerificationFrame::initial_frame("Foo", "m", "()V", true, 1, 4);
        frame.push(VType::Int).unwrap();
        frame.push(VType::Float).unwrap();
        frame.clear_stack();
        assert!(frame.stack.is_empty());
    }
}
