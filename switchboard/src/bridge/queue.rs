//! A bounded blocking FIFO and a one-way flag that threads can wait on.

use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A bounded FIFO with non-blocking and timed puts and gets.
pub struct BoundedQueue<T> {
    items: Mutex<VecDeque<T>>,
    not_empty: Condvar,
    not_full: Condvar,
    max: usize,
}

impl<T> BoundedQueue<T> {
    pub fn new(max: usize) -> Self {
        BoundedQueue {
            items: Mutex::new(VecDeque::new()),
            not_empty: Condvar::new(),
            not_full: Condvar::new(),
            max,
        }
    }

    /// Add without waiting: Err(item) when full.
    pub fn put_nowait(&self, item: T) -> Result<(), T> {
        let mut items = self.items.lock().expect("queue");
        if items.len() >= self.max {
            return Err(item);
        }
        items.push_back(item);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Add, waiting up to `timeout` for room: Err(item) if none appears in time.
    pub fn put_timeout(&self, item: T, timeout: Duration) -> Result<(), T> {
        let deadline = Instant::now() + timeout;
        let mut items = self.items.lock().expect("queue");
        while items.len() >= self.max {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(item);
            }
            items = self.not_full.wait_timeout(items, left).expect("queue").0;
        }
        items.push_back(item);
        self.not_empty.notify_one();
        Ok(())
    }

    /// Take the oldest item, waiting up to `timeout`; None on timeout.
    pub fn get_timeout(&self, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;
        let mut items = self.items.lock().expect("queue");
        loop {
            if let Some(item) = items.pop_front() {
                self.not_full.notify_one();
                return Some(item);
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            items = self.not_empty.wait_timeout(items, left).expect("queue").0;
        }
    }

    /// Take the oldest item without waiting; None when empty.
    pub fn get_nowait(&self) -> Option<T> {
        let mut items = self.items.lock().expect("queue");
        let item = items.pop_front();
        if item.is_some() {
            self.not_full.notify_one();
        }
        item
    }
}

/// A flag that starts clear, is set once, and can be waited on with a timeout.
#[derive(Default)]
pub struct Flag {
    set: Mutex<bool>,
    changed: Condvar,
}

impl Flag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self) {
        *self.set.lock().expect("flag") = true;
        self.changed.notify_all();
    }

    pub fn is_set(&self) -> bool {
        *self.set.lock().expect("flag")
    }

    /// Wait until the flag is set, however long that takes.
    pub fn wait_forever(&self) {
        let mut set = self.set.lock().expect("flag");
        while !*set {
            set = self.changed.wait(set).expect("flag");
        }
    }

    /// Wait up to `timeout` for the flag: true once set.
    pub fn wait(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut set = self.set.lock().expect("flag");
        while !*set {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            set = self.changed.wait_timeout(set, left).expect("flag").0;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_is_bounded_and_fifo() {
        let queue = BoundedQueue::new(2);
        assert!(queue.put_nowait(1).is_ok());
        assert!(queue.put_nowait(2).is_ok());
        assert_eq!(queue.put_nowait(3), Err(3));
        assert_eq!(queue.put_timeout(3, Duration::from_millis(10)), Err(3));
        assert_eq!(queue.get_nowait(), Some(1));
        assert_eq!(queue.get_timeout(Duration::from_millis(10)), Some(2));
        assert_eq!(queue.get_timeout(Duration::from_millis(10)), None);
        let flag = Flag::new();
        assert!(!flag.wait(Duration::from_millis(5)));
        flag.set();
        assert!(flag.wait(Duration::from_secs(5)));
    }
}
