use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

enum Control<T> {
    Elem(T),
    Unblock,
}

pub struct MessagesQueue<T>
where
    T: Send,
{
    queue: Mutex<VecDeque<Control<T>>>,
    condvar: Condvar,
}

impl<T> MessagesQueue<T>
where
    T: Send,
{
    pub fn with_capacity(capacity: usize) -> Arc<MessagesQueue<T>> {
        Arc::new(MessagesQueue {
            queue: Mutex::new(VecDeque::with_capacity(capacity)),
            condvar: Condvar::new(),
        })
    }

    /// Pushes an element to the queue.
    pub fn push(&self, value: T) {
        let mut queue = self.queue.lock().unwrap();
        queue.push_back(Control::Elem(value));
        self.condvar.notify_one();
    }

    /// Unblock one thread stuck in pop loop.
    pub fn unblock(&self) {
        let mut queue = self.queue.lock().unwrap();
        queue.push_back(Control::Unblock);
        self.condvar.notify_one();
    }

    /// Pops an element. Blocks until one is available.
    /// Returns None in case unblock() was issued.
    pub fn pop(&self) -> Option<T> {
        let mut queue = self.queue.lock().unwrap();

        loop {
            match queue.pop_front() {
                Some(Control::Elem(value)) => return Some(value),
                Some(Control::Unblock) => return None,
                None => (),
            }

            queue = self.condvar.wait(queue).unwrap();
        }
    }

    /// Tries to pop an element without blocking.
    pub fn try_pop(&self) -> Option<T> {
        let mut queue = self.queue.lock().unwrap();
        match queue.pop_front() {
            Some(Control::Elem(value)) => Some(value),
            Some(Control::Unblock) | None => None,
        }
    }

    /// Tries to pop an element without blocking
    /// more than the specified timeout duration
    /// or unblock() was issued
    pub fn pop_timeout(&self, timeout: Duration) -> Option<T> {
        let mut queue = self.queue.lock().unwrap();
        let start = Instant::now();
        loop {
            match queue.pop_front() {
                Some(Control::Elem(value)) => return Some(value),
                Some(Control::Unblock) => return None,
                None => (),
            }
            let remaining = timeout.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                return None;
            }
            let (next_queue, result) = self.condvar.wait_timeout(queue, remaining).unwrap();
            queue = next_queue;
            if result.timed_out() {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unrelated_wakeups_do_not_restart_receive_timeout() {
        let queue = MessagesQueue::<usize>::with_capacity(1);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let notifier_queue = queue.clone();
        let notifier_stop = stop.clone();
        let notifier = std::thread::spawn(move || {
            while !notifier_stop.load(std::sync::atomic::Ordering::Relaxed) {
                notifier_queue.condvar.notify_all();
                std::thread::sleep(Duration::from_millis(1));
            }
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let receiving = queue.clone();
        let reader = std::thread::spawn(move || {
            tx.send(receiving.pop_timeout(Duration::from_millis(30))).unwrap();
        });
        let result = rx.recv_timeout(Duration::from_secs(2));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        notifier.join().unwrap();
        reader.join().unwrap();
        assert_eq!(result.unwrap(), None, "wakeups extended the receive deadline");
        queue.push(7);
        assert_eq!(queue.pop_timeout(Duration::ZERO), Some(7));
        assert_eq!(queue.pop_timeout(Duration::ZERO), None);
    }
}
