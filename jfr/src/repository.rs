// AUDIT 2026-05-16: std HashMap unused (replaced by FxHashMap below).
use std::collections::VecDeque;

use rustc_hash::FxHashMap;

use crate::event::{EventInstance, EventTypeId};

/// In-memory event storage with ring-buffer eviction.
///
/// Uses a `VecDeque` for O(1) front eviction and maintains a secondary
/// `type_index` mapping `EventTypeId` to buffer indices for O(1) type queries.
pub struct EventRepository {
    events: VecDeque<EventInstance>,
    max_events: usize,
    total_recorded: u64,
    /// Index from event type_id to the set of logical indices (offset from
    /// `total_recorded - events.len()`). Maintained on push/evict/clear.
    /// T10.9.B: FxHashMap — EventTypeId is internal.
    type_index: FxHashMap<EventTypeId, Vec<usize>>,
    /// The absolute index of the first element currently in `events`.
    /// Equals `total_recorded - events.len()` after each push.
    base_index: u64,
}

impl EventRepository {
    pub fn new(max_events: usize) -> Self {
        Self {
            events: VecDeque::new(),
            max_events,
            total_recorded: 0,
            type_index: FxHashMap::default(),
            base_index: 0,
        }
    }

    /// Push an event, evicting the oldest if over the limit. O(1) amortized.
    pub fn push(&mut self, event: EventInstance) {
        if self.events.len() >= self.max_events {
            // Evict oldest (front) — O(1) with VecDeque
            if let Some(evicted) = self.events.pop_front() {
                // Remove evicted event from type index
                if let Some(indices) = self.type_index.get_mut(&evicted.type_id) {
                    if let Some(pos) = indices.iter().position(|&i| i == self.base_index as usize) {
                        indices.swap_remove(pos);
                    }
                    if indices.is_empty() {
                        self.type_index.remove(&evicted.type_id);
                    }
                }
                self.base_index += 1;
            }
        }
        // Add to type index
        let abs_index = self.total_recorded as usize;
        self.type_index
            .entry(event.type_id)
            .or_default()
            .push(abs_index);

        self.events.push_back(event);
        self.total_recorded += 1;
    }

    /// Return a slice-like view of all current events.
    /// Note: `VecDeque::make_contiguous` is called to allow returning a slice.
    pub fn events(&mut self) -> &[EventInstance] {
        self.events.make_contiguous();
        let (front, _) = self.events.as_slices();
        front
    }

    /// Return an iterator over all current events (does not require contiguous layout).
    pub fn iter(&self) -> impl Iterator<Item = &EventInstance> {
        self.events.iter()
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Total number of events ever recorded, including evicted ones.
    pub fn total_recorded(&self) -> u64 {
        self.total_recorded
    }

    pub fn clear(&mut self) {
        self.events.clear();
        self.type_index.clear();
        self.base_index = self.total_recorded;
    }

    /// Return references to events matching the given type id.
    /// Uses the type_index for O(1) lookup of matching indices.
    pub fn events_by_type(&self, type_id: EventTypeId) -> Vec<&EventInstance> {
        match self.type_index.get(&type_id) {
            Some(abs_indices) => {
                abs_indices
                    .iter()
                    .filter_map(|&abs_idx| {
                        let rel = abs_idx.checked_sub(self.base_index as usize)?;
                        self.events.get(rel)
                    })
                    .collect()
            }
            None => vec![],
        }
    }

    /// Return references to events whose start_time falls within [start, end].
    pub fn events_in_range(&self, start: u64, end: u64) -> Vec<&EventInstance> {
        self.events
            .iter()
            .filter(|e| e.start_time >= start && e.start_time <= end)
            .collect()
    }
}

impl Default for EventRepository {
    fn default() -> Self {
        Self::new(100_000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_event(type_id: EventTypeId, start: u64, end: u64) -> EventInstance {
        EventInstance {
            type_id,
            start_time: start,
            end_time: end,
            thread_id: 1,
            fields: vec![],
        }
    }

    #[test]
    fn test_new_repository() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
        assert_eq!(repo.total_recorded(), 0);
    }

    #[test]
    fn test_default_repository() {
        let repo = EventRepository::default();
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
    }

    #[test]
    fn test_push_single() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        assert_eq!(repo.len(), 1);
        assert!(!repo.is_empty());
        assert_eq!(repo.total_recorded(), 1);
    }

    #[test]
    fn test_push_multiple() {
        let mut repo = EventRepository::new(10);
        for i in 0..5 {
            repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(repo.len(), 5);
        assert_eq!(repo.total_recorded(), 5);
    }

    #[test]
    fn test_eviction_at_capacity() {
        let mut repo = EventRepository::new(3);
        for i in 0..5u64 {
            repo.push(make_event(EventTypeId(1), i * 100, i * 100 + 50));
        }
        assert_eq!(repo.len(), 3);
        assert_eq!(repo.total_recorded(), 5);
        // Oldest events should be evicted
        let events = repo.events();
        assert_eq!(events[0].start_time, 200);
        assert_eq!(events[1].start_time, 300);
        assert_eq!(events[2].start_time, 400);
    }

    #[test]
    fn test_eviction_capacity_one() {
        let mut repo = EventRepository::new(1);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(1), 300, 400));
        assert_eq!(repo.len(), 1);
        assert_eq!(repo.total_recorded(), 2);
        assert_eq!(repo.events()[0].start_time, 300);
    }

    #[test]
    fn test_events_returns_slice() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 300, 400));
        let events = repo.events();
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].type_id, EventTypeId(1));
        assert_eq!(events[1].type_id, EventTypeId(2));
    }

    #[test]
    fn test_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(1), 200, 300));
        assert_eq!(repo.len(), 2);
        repo.clear();
        assert_eq!(repo.len(), 0);
        assert!(repo.is_empty());
        // total_recorded is NOT cleared by clear()
        assert_eq!(repo.total_recorded(), 2);
    }

    #[test]
    fn test_events_by_type_single_type() {
        let mut repo = EventRepository::new(10);
        let t1 = EventTypeId(1);
        repo.push(make_event(t1, 100, 200));
        repo.push(make_event(t1, 200, 300));
        let matched = repo.events_by_type(t1);
        assert_eq!(matched.len(), 2);
    }

    #[test]
    fn test_events_by_type_mixed() {
        let mut repo = EventRepository::new(10);
        let t1 = EventTypeId(1);
        let t2 = EventTypeId(2);
        let t3 = EventTypeId(3);
        repo.push(make_event(t1, 100, 200));
        repo.push(make_event(t2, 200, 300));
        repo.push(make_event(t1, 300, 400));
        repo.push(make_event(t3, 400, 500));
        assert_eq!(repo.events_by_type(t1).len(), 2);
        assert_eq!(repo.events_by_type(t2).len(), 1);
        assert_eq!(repo.events_by_type(t3).len(), 1);
    }

    #[test]
    fn test_events_by_type_none_found() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        assert_eq!(repo.events_by_type(EventTypeId(99)).len(), 0);
    }

    #[test]
    fn test_events_by_type_empty_repo() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
    }

    #[test]
    fn test_events_in_range_inclusive() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        repo.push(make_event(EventTypeId(1), 200, 250));
        repo.push(make_event(EventTypeId(1), 300, 350));
        // Range [200, 300] should include events at start_time 200 and 300
        let in_range = repo.events_in_range(200, 300);
        assert_eq!(in_range.len(), 2);
    }

    #[test]
    fn test_events_in_range_none_match() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        repo.push(make_event(EventTypeId(1), 500, 550));
        let in_range = repo.events_in_range(200, 400);
        assert_eq!(in_range.len(), 0);
    }

    #[test]
    fn test_events_in_range_exact_boundary() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 150));
        // Range [100, 100] should include event at start_time 100
        let in_range = repo.events_in_range(100, 100);
        assert_eq!(in_range.len(), 1);
    }

    #[test]
    fn test_events_in_range_empty_repo() {
        let repo = EventRepository::new(10);
        assert_eq!(repo.events_in_range(0, 1000).len(), 0);
    }

    #[test]
    fn test_total_recorded_persists_after_eviction() {
        let mut repo = EventRepository::new(2);
        for i in 0..10u64 {
            repo.push(make_event(EventTypeId(1), i, i + 1));
        }
        assert_eq!(repo.len(), 2);
        assert_eq!(repo.total_recorded(), 10);
    }

    #[test]
    fn test_push_after_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.clear();
        repo.push(make_event(EventTypeId(2), 300, 400));
        assert_eq!(repo.len(), 1);
        assert_eq!(repo.events()[0].type_id, EventTypeId(2));
    }

    #[test]
    fn test_type_index_after_eviction() {
        let mut repo = EventRepository::new(2);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 200, 300));
        repo.push(make_event(EventTypeId(1), 300, 400));
        // First event (type 1) was evicted, only the new type 1 remains
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 1);
        assert_eq!(repo.events_by_type(EventTypeId(2)).len(), 1);
    }

    #[test]
    fn test_type_index_cleared_on_clear() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.clear();
        assert_eq!(repo.events_by_type(EventTypeId(1)).len(), 0);
    }

    #[test]
    fn test_iter() {
        let mut repo = EventRepository::new(10);
        repo.push(make_event(EventTypeId(1), 100, 200));
        repo.push(make_event(EventTypeId(2), 200, 300));
        let collected: Vec<_> = repo.iter().collect();
        assert_eq!(collected.len(), 2);
        assert_eq!(collected[0].type_id, EventTypeId(1));
        assert_eq!(collected[1].type_id, EventTypeId(2));
    }
}
