// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The JVM call stack (thread execution stack).
//!
//! Each thread has its own call stack that tracks method invocation metadata.
//! This provides the information needed for stack traces, debugging, and
//! `StackOverflowError` detection.

/// Metadata about a single stack frame entry.
#[derive(Debug, Clone)]
pub struct StackFrameEntry {
    /// Fully qualified class name (e.g., "java/lang/String").
    pub class_name: String,
    /// Method name (e.g., "charAt").
    pub method_name: String,
    /// Method descriptor (e.g., "(I)C").
    pub descriptor: String,
    /// Source file name, if available.
    pub source_file: Option<String>,
    /// Current bytecode program counter within the method.
    pub pc: usize,
    /// Line number in source, or -1 if unavailable.
    pub line_number: i32,
    /// Whether this is a native method.
    pub is_native: bool,
}

/// The JVM call stack (thread execution stack).
///
/// Each thread has its own call stack containing metadata for active method
/// invocations. Used for stack traces, debugging, and `StackOverflowError`
/// detection.
#[derive(Debug)]
pub struct CallStack {
    max_depth: usize,
    frames: Vec<StackFrameEntry>,
}

impl CallStack {
    /// Create a new, empty call stack with the given maximum depth.
    pub fn new(max_depth: usize) -> Self {
        Self {
            max_depth,
            frames: Vec::with_capacity(64.min(max_depth)),
        }
    }

    /// Push a new frame onto the call stack.
    ///
    /// Returns `Err` if the stack would exceed its maximum depth
    /// (indicating a `StackOverflowError`).
    pub fn push(&mut self, entry: StackFrameEntry) -> Result<(), StackOverflowError> {
        if self.frames.len() >= self.max_depth {
            return Err(StackOverflowError {
                depth: self.max_depth,
                method: format!("{}.{}", entry.class_name, entry.method_name),
            });
        }
        self.frames.push(entry);
        Ok(())
    }

    /// Pop the top frame from the call stack.
    ///
    /// Returns `None` if the stack is empty.
    pub fn pop(&mut self) -> Option<StackFrameEntry> {
        self.frames.pop()
    }

    /// Get the current stack depth.
    pub fn depth(&self) -> usize {
        self.frames.len()
    }

    /// Get the maximum allowed depth.
    pub fn max_depth(&self) -> usize {
        self.max_depth
    }

    /// Check if the stack is at its maximum depth.
    pub fn is_full(&self) -> bool {
        self.frames.len() >= self.max_depth
    }

    /// Check if the stack is empty.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// Get the top frame without removing it.
    pub fn peek(&self) -> Option<&StackFrameEntry> {
        self.frames.last()
    }

    /// Get a mutable reference to the top frame.
    pub fn peek_mut(&mut self) -> Option<&mut StackFrameEntry> {
        self.frames.last_mut()
    }

    /// Get a frame at a specific depth (0 = bottom, depth-1 = top).
    pub fn get(&self, index: usize) -> Option<&StackFrameEntry> {
        self.frames.get(index)
    }

    /// Iterate over all frames from bottom to top.
    pub fn iter(&self) -> impl Iterator<Item = &StackFrameEntry> {
        self.frames.iter()
    }

    /// Iterate over all frames from top to bottom (most recent first).
    pub fn iter_top_down(&self) -> impl Iterator<Item = &StackFrameEntry> {
        self.frames.iter().rev()
    }

    /// Generate a stack trace string (similar to `Throwable.printStackTrace()`).
    pub fn stack_trace(&self) -> String {
        let mut output = String::new();
        for frame in self.frames.iter().rev() {
            let location = if frame.is_native {
                "Native Method".to_string()
            } else if let Some(ref file) = frame.source_file {
                if frame.line_number >= 0 {
                    format!("{}:{}", file, frame.line_number)
                } else {
                    file.clone()
                }
            } else {
                "Unknown Source".to_string()
            };
            output.push_str(&format!(
                "\tat {}.{}({})\n",
                frame.class_name.replace('/', "."),
                frame.method_name,
                location,
            ));
        }
        output
    }

    /// Update the program counter of the top frame.
    pub fn update_pc(&mut self, pc: usize) {
        if let Some(top) = self.frames.last_mut() {
            top.pc = pc;
        }
    }

    /// Update the line number of the top frame.
    pub fn update_line_number(&mut self, line: i32) {
        if let Some(top) = self.frames.last_mut() {
            top.line_number = line;
        }
    }

    /// Clear all frames from the stack.
    pub fn clear(&mut self) {
        self.frames.clear();
    }

    /// Get all frames as a slice (bottom to top).
    pub fn as_slice(&self) -> &[StackFrameEntry] {
        &self.frames
    }
}

/// Error returned when pushing a frame would exceed the maximum stack depth.
#[derive(Debug, Clone)]
pub struct StackOverflowError {
    pub depth: usize,
    pub method: String,
}

impl std::fmt::Display for StackOverflowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "StackOverflowError: stack depth {} exceeded in {}",
            self.depth, self.method
        )
    }
}

impl std::error::Error for StackOverflowError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_frame(class: &str, method: &str) -> StackFrameEntry {
        StackFrameEntry {
            class_name: class.to_string(),
            method_name: method.to_string(),
            descriptor: "()V".to_string(),
            source_file: Some(format!("{}.java", class.split('/').last().unwrap_or(class))),
            pc: 0,
            line_number: 1,
            is_native: false,
        }
    }

    #[test]
    fn test_new_stack() {
        let stack = CallStack::new(512);
        assert_eq!(stack.depth(), 0);
        assert_eq!(stack.max_depth(), 512);
        assert!(stack.is_empty());
        assert!(!stack.is_full());
    }

    #[test]
    fn test_push_pop() {
        let mut stack = CallStack::new(10);
        stack
            .push(sample_frame("java/lang/Object", "init"))
            .unwrap();
        stack
            .push(sample_frame("com/example/Main", "main"))
            .unwrap();
        assert_eq!(stack.depth(), 2);

        let top = stack.pop().unwrap();
        assert_eq!(top.method_name, "main");
        assert_eq!(stack.depth(), 1);

        let bottom = stack.pop().unwrap();
        assert_eq!(bottom.method_name, "init");
        assert!(stack.is_empty());
    }

    #[test]
    fn test_stack_overflow() {
        let mut stack = CallStack::new(3);
        stack.push(sample_frame("A", "a")).unwrap();
        stack.push(sample_frame("B", "b")).unwrap();
        stack.push(sample_frame("C", "c")).unwrap();
        assert!(stack.is_full());

        let err = stack.push(sample_frame("D", "d")).unwrap_err();
        assert_eq!(err.depth, 3);
        assert!(err.method.contains("D.d"));
        assert_eq!(stack.depth(), 3); // unchanged
    }

    #[test]
    fn test_peek() {
        let mut stack = CallStack::new(10);
        assert!(stack.peek().is_none());
        stack.push(sample_frame("A", "a")).unwrap();
        assert_eq!(stack.peek().unwrap().method_name, "a");
    }

    #[test]
    fn test_get_by_index() {
        let mut stack = CallStack::new(10);
        stack.push(sample_frame("A", "a")).unwrap();
        stack.push(sample_frame("B", "b")).unwrap();
        assert_eq!(stack.get(0).unwrap().method_name, "a");
        assert_eq!(stack.get(1).unwrap().method_name, "b");
        assert!(stack.get(2).is_none());
    }

    #[test]
    fn test_stack_trace() {
        let mut stack = CallStack::new(10);
        stack
            .push(sample_frame("java/lang/Object", "<init>"))
            .unwrap();
        stack
            .push(StackFrameEntry {
                class_name: "com/example/Main".to_string(),
                method_name: "main".to_string(),
                descriptor: "([Ljava/lang/String;)V".to_string(),
                source_file: Some("Main.java".to_string()),
                pc: 5,
                line_number: 42,
                is_native: false,
            })
            .unwrap();

        let trace = stack.stack_trace();
        assert!(trace.contains("com.example.Main.main(Main.java:42)"));
        assert!(trace.contains("java.lang.Object.<init>"));
    }

    #[test]
    fn test_update_pc_and_line() {
        let mut stack = CallStack::new(10);
        stack.push(sample_frame("A", "a")).unwrap();
        stack.update_pc(42);
        stack.update_line_number(100);
        assert_eq!(stack.peek().unwrap().pc, 42);
        assert_eq!(stack.peek().unwrap().line_number, 100);
    }

    #[test]
    fn test_clear() {
        let mut stack = CallStack::new(10);
        stack.push(sample_frame("A", "a")).unwrap();
        stack.push(sample_frame("B", "b")).unwrap();
        stack.clear();
        assert!(stack.is_empty());
        assert_eq!(stack.depth(), 0);
    }

    #[test]
    fn test_iter_top_down() {
        let mut stack = CallStack::new(10);
        stack.push(sample_frame("A", "a")).unwrap();
        stack.push(sample_frame("B", "b")).unwrap();
        stack.push(sample_frame("C", "c")).unwrap();
        let names: Vec<&str> = stack
            .iter_top_down()
            .map(|f| f.method_name.as_str())
            .collect();
        assert_eq!(names, vec!["c", "b", "a"]);
    }

    #[test]
    fn test_native_method_in_trace() {
        let mut stack = CallStack::new(10);
        stack
            .push(StackFrameEntry {
                class_name: "java/lang/Thread".to_string(),
                method_name: "sleep".to_string(),
                descriptor: "(J)V".to_string(),
                source_file: None,
                pc: 0,
                line_number: -1,
                is_native: true,
            })
            .unwrap();
        let trace = stack.stack_trace();
        assert!(trace.contains("Native Method"));
    }

    #[test]
    fn test_pop_empty() {
        let mut stack = CallStack::new(10);
        assert!(stack.pop().is_none());
    }

    #[test]
    fn test_overflow_error_display() {
        let err = StackOverflowError {
            depth: 512,
            method: "com/example/Recursive.recurse".to_string(),
        };
        let msg = format!("{}", err);
        assert!(msg.contains("512"));
        assert!(msg.contains("Recursive.recurse"));
    }
}
