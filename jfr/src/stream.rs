// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

//! Live event stream for consuming JFR events as they are emitted.
//!
//! `EventStream` provides an iterator/poll interface over a recording's event
//! repository.  It tracks a reader cursor so that each call to `poll` or
//! `next_event` returns only events that have been added since the last read.
//!
//! Optionally, an `EventStream` can be configured with:
//! - A set of event type filters (only those types are delivered)
//! - One or more callbacks that are invoked for each matching event

// AUDIT 2026-05-16: std HashSet unused (replaced by FxHashSet below).

use rustc_hash::FxHashSet;

use crate::event::{EventInstance, EventTypeId};
use crate::repository::EventRepository;

/// A callback that is invoked for each event delivered by the stream.
pub type EventCallback = Box<dyn FnMut(&EventInstance) + Send>;

/// Live event stream that tracks a read cursor over an `EventRepository`.
///
/// Usage pattern:
/// ```ignore
/// let mut stream = EventStream::new();
/// stream.add_filter(my_event_type_id);
/// // ... later, in a poll loop:
/// let events = stream.poll(&repo);
/// for event in &events {
///     // process event
/// }
/// ```
pub struct EventStream {
    /// Absolute index of the next event to read (matches `EventRepository::total_recorded`).
    read_index: u64,
    /// If non-empty, only events matching these type IDs are delivered.
    /// T10.9.B: FxHashSet — EventTypeId is internal.
    type_filters: FxHashSet<EventTypeId>,
    /// Registered callbacks invoked for every matching event during `poll`.
    callbacks: Vec<EventCallback>,
    /// Whether the stream is open.  Once closed, `poll` returns empty.
    closed: bool,
}

impl EventStream {
    /// Create a new event stream starting from the current end of the repository.
    ///
    /// Events that were already in the repository before this call are *not*
    /// delivered.  To read from the beginning, use `new_from_start`.
    pub fn new(repo: &EventRepository) -> Self {
        Self {
            read_index: repo.total_recorded(),
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        }
    }

    /// Create a new event stream that starts reading from the very beginning
    /// of the repository (including events already present).
    pub fn new_from_start() -> Self {
        Self {
            read_index: 0,
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        }
    }

    /// Add a type filter.  When at least one filter is set, only events whose
    /// `type_id` is in the filter set are delivered.
    pub fn add_filter(&mut self, type_id: EventTypeId) {
        self.type_filters.insert(type_id);
    }

    /// Remove a previously added type filter.
    pub fn remove_filter(&mut self, type_id: &EventTypeId) {
        self.type_filters.remove(type_id);
    }

    /// Clear all type filters (all event types will be delivered).
    pub fn clear_filters(&mut self) {
        self.type_filters.clear();
    }

    /// Returns true if this stream has been closed.
    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Close the stream.  Subsequent calls to `poll` / `next_event` return empty/None.
    pub fn close(&mut self) {
        self.closed = true;
        self.callbacks.clear();
    }

    /// Register a callback that will be invoked for each matching event during `poll`.
    pub fn on_event(&mut self, callback: EventCallback) {
        self.callbacks.push(callback);
    }

    /// Returns the number of registered callbacks.
    pub fn callback_count(&self) -> usize {
        self.callbacks.len()
    }

    /// Returns the current read cursor position (absolute index).
    pub fn read_position(&self) -> u64 {
        self.read_index
    }

    /// Poll the repository for new events since the last read.
    ///
    /// Returns cloned copies of all matching new events and advances the
    /// read cursor.  Also invokes any registered callbacks for each event.
    ///
    /// If the stream is closed, returns an empty vector.
    pub fn poll(&mut self, repo: &EventRepository) -> Vec<EventInstance> {
        if self.closed {
            return Vec::new();
        }

        let total = repo.total_recorded();
        if self.read_index >= total {
            return Vec::new();
        }

        let repo_len = repo.len() as u64;
        let base_index = total - repo_len;

        // If our read cursor has fallen behind the ring buffer (events were evicted),
        // jump forward to the oldest available event.
        let effective_start = if self.read_index < base_index {
            base_index
        } else {
            self.read_index
        };

        let relative_start = (effective_start - base_index) as usize;
        // Perf: previously `repo.iter().skip(relative_start)` walked the
        // VecDeque from the front on every poll — O(n) even when the cursor
        // was already near the end. `EventRepository::get` indexes the
        // backing VecDeque in O(1) (same fix already applied to
        // `next_event`). We iterate `[relative_start, repo_len)` directly.
        let repo_len_usize = repo.len();
        let mut results = Vec::with_capacity(repo_len_usize.saturating_sub(relative_start));

        for rel in relative_start..repo_len_usize {
            let event = match repo.get(rel) {
                Some(e) => e,
                None => break,
            };
            if !self.type_filters.is_empty() && !self.type_filters.contains(&event.type_id) {
                continue;
            }
            // Invoke callbacks
            for cb in &mut self.callbacks {
                cb(event);
            }
            results.push(event.clone());
        }

        self.read_index = total;
        results
    }

    /// Open a previously-dumped JFR recording on disk and populate a fresh
    /// in-memory `EventRepository` from its event records, so that the rest of
    /// the streaming API (`poll`, `next_event`, type filters, callbacks) works
    /// against it unchanged. This is the Rust analogue of
    /// `jdk.jfr.consumer.EventStream.openRepository(Path)`.
    ///
    /// The `registry` must describe every event type in the file; callers
    /// typically pass the same registry used to dump, or a registry populated
    /// via `register_builtin_events` for built-in event files.
    ///
    /// Returns the populated repository and a stream whose cursor is at the
    /// beginning so that every event on disk is delivered exactly once.
    pub fn open_repository(
        path: &std::path::Path,
        registry: &crate::event::EventTypeRegistry,
    ) -> Result<(EventRepository, Self), crate::dump::JfrDumpError> {
        let events = crate::dump::read_events(path, registry)?;
        // Capacity: fit every event (no eviction for replay).
        let mut repo = EventRepository::new(events.len().max(1));
        for ev in events {
            repo.push(ev);
        }
        let stream = Self {
            read_index: 0,
            type_filters: FxHashSet::default(),
            callbacks: Vec::new(),
            closed: false,
        };
        Ok((repo, stream))
    }

    /// Return the next single matching event, or `None` if no new events are available.
    ///
    /// This is a convenience method that advances the cursor by one event at a time.
    /// Non-matching events (filtered out by type) are skipped but still advance the cursor.
    pub fn next_event(&mut self, repo: &EventRepository) -> Option<EventInstance> {
        if self.closed {
            return None;
        }

        let total = repo.total_recorded();
        if self.read_index >= total {
            return None;
        }

        let repo_len = repo.len() as u64;
        let base_index = total - repo_len;

        // Jump past evicted events
        if self.read_index < base_index {
            self.read_index = base_index;
        }

        // Round-5 HIGH-fix (Bug 5, 2026-05-17): previously this loop called
        // `repo.iter().nth(rel)` per iteration, which walks the VecDeque
        // from the front every call — quadratic over a full filtered drain.
        // `EventRepository::get(rel)` is O(1).
        while self.read_index < total {
            let rel = (self.read_index - base_index) as usize;
            self.read_index += 1;

            if let Some(event) = repo.get(rel) {
                if self.type_filters.is_empty() || self.type_filters.contains(&event.type_id) {
                    for cb in &mut self.callbacks {
                        cb(event);
                    }
                    return Some(event.clone());
                }
            }
        }

        None
    }
}

#[cfg(test)]
mod tests;
