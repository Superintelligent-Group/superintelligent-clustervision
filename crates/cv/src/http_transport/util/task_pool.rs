use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::Duration;

/// Manages a collection of threads.
///
/// A new thread is created every time all the existing threads are full.
/// Any idle thread will automatically die after a few seconds.
pub struct TaskPool {
    sharing: Arc<Sharing>,
}

struct Sharing {
    // list of the tasks to be done by worker threads
    todo: Mutex<VecDeque<Box<dyn FnMut() + Send>>>,

    // condvar that will be notified whenever a task is added to `todo`
    condvar: Condvar,

    // number of total worker threads running
    active_tasks: AtomicUsize,

    // number of idle worker threads
    waiting_tasks: AtomicUsize,
}

/// Minimum number of active threads.
static MIN_THREADS: usize = 4;

struct Registration<'a> {
    nb: &'a AtomicUsize,
}

impl<'a> Registration<'a> {
    fn new(nb: &'a AtomicUsize) -> Registration<'a> {
        nb.fetch_add(1, Ordering::Release);
        Registration { nb }
    }
}

impl<'a> Drop for Registration<'a> {
    fn drop(&mut self) {
        self.nb.fetch_sub(1, Ordering::Release);
    }
}

impl TaskPool {
    pub fn new() -> TaskPool {
        let pool = TaskPool {
            sharing: Arc::new(Sharing {
                todo: Mutex::new(VecDeque::new()),
                condvar: Condvar::new(),
                active_tasks: AtomicUsize::new(0),
                waiting_tasks: AtomicUsize::new(0),
            }),
        };

        for _ in 0..MIN_THREADS {
            pool.add_thread(None)
        }

        pool
    }

    /// Executes a function in a thread.
    /// If no thread is available, spawns a new one.
    pub fn spawn(&self, code: Box<dyn FnMut() + Send>) {
        let mut queue = self.sharing.todo.lock().unwrap();

        self.dispatch_locked(code, &mut queue);
    }

    // The mutex protects both queued reservations and transitions into/out of waiting.
    // A notified worker has not consumed its queued connection until it reacquires this lock.
    fn dispatch_locked(&self, code: Box<dyn FnMut() + Send>, queue: &mut VecDeque<Box<dyn FnMut() + Send>>) {
        if queue.len() >= self.sharing.waiting_tasks.load(Ordering::Acquire) {
            self.add_thread(Some(code));
        } else {
            queue.push_back(code);
            self.sharing.condvar.notify_one();
        }
    }

    fn add_thread(&self, initial_fn: Option<Box<dyn FnMut() + Send>>) {
        let sharing = self.sharing.clone();

        thread::spawn(move || {
            let sharing = sharing;
            let _active_guard = Registration::new(&sharing.active_tasks);

            if let Some(mut f) = initial_fn {
                f();
            }

            loop {
                let mut task: Box<dyn FnMut() + Send> = {
                    let mut todo = sharing.todo.lock().unwrap();

                    let task;
                    loop {
                        if let Some(poped_task) = todo.pop_front() {
                            task = poped_task;
                            break;
                        }
                        let _waiting_guard = Registration::new(&sharing.waiting_tasks);

                        let received = if sharing.active_tasks.load(Ordering::Acquire) <= MIN_THREADS {
                            todo = sharing.condvar.wait(todo).unwrap();
                            true
                        } else {
                            let (new_lock, waitres) =
                                sharing.condvar.wait_timeout(todo, Duration::from_millis(5000)).unwrap();
                            todo = new_lock;
                            !waitres.timed_out()
                        };

                        if !received && todo.is_empty() {
                            return;
                        }
                    }

                    task
                };

                task();
            }
        });
    }
}

impl Drop for TaskPool {
    fn drop(&mut self) {
        self.sharing.active_tasks.store(999_999_999, Ordering::Release);
        self.sharing.condvar.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn idle_pool() -> TaskPool {
        let pool = TaskPool::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        loop {
            let queue = pool.sharing.todo.lock().unwrap();
            if pool.sharing.waiting_tasks.load(Ordering::Acquire) == MIN_THREADS {
                drop(queue);
                return pool;
            }
            drop(queue);
            assert!(
                std::time::Instant::now() < deadline,
                "pool workers did not reach their idle condition"
            );
            thread::yield_now();
        }
    }

    #[test]
    fn queued_connections_reserve_idle_workers_before_a_burst_can_overcommit_them() {
        let pool = idle_pool();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let (started, starts) = mpsc::channel();
        let (done, finishes) = mpsc::channel();
        {
            // Force the allowed producer interleaving: no notified worker can consume a queue
            // reservation while the acceptor dispatches this burst under the same mutex.
            let mut queue = pool.sharing.todo.lock().unwrap();
            for id in 0..MIN_THREADS + 1 {
                let release = release.clone();
                let started = started.clone();
                let done = done.clone();
                pool.dispatch_locked(
                    Box::new(move || {
                        started.send(id).unwrap();
                        let mut ready = release.0.lock().unwrap();
                        while !*ready {
                            ready = release.1.wait(ready).unwrap();
                        }
                        done.send(id).unwrap();
                    }),
                    &mut queue,
                );
            }
        }
        let mut running = Vec::new();
        for _ in 0..MIN_THREADS + 1 {
            match starts.recv_timeout(Duration::from_secs(2)) {
                Ok(id) => running.push(id),
                Err(_) => break,
            }
        }
        *release.0.lock().unwrap() = true;
        release.1.notify_all();
        for _ in 0..MIN_THREADS + 1 {
            finishes.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert_eq!(
            running.len(),
            MIN_THREADS + 1,
            "queued keepalive jobs must not share one idle-worker reservation"
        );
    }

    #[test]
    fn preconnections_cannot_strand_a_new_http_connection_or_its_keepalive_reuse() {
        use crate::http_transport::client::ClientConnection;
        use crate::http_transport::connection::Connection;
        use crate::http_transport::util::RefinedTcpStream;
        use crate::http_transport::Response;
        use std::io::{Read, Write};
        use std::net::{Shutdown, TcpListener, TcpStream};
        let pool = idle_pool();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut clients = Vec::new();
        let (done, finishes) = mpsc::channel();
        {
            let mut queue = pool.sharing.todo.lock().unwrap();
            for id in 0..MIN_THREADS + 1 {
                let client = TcpStream::connect(addr).unwrap();
                let (server, _) = listener.accept().unwrap();
                let (read, write) = RefinedTcpStream::new(Connection::from(server));
                let mut connection = Some(ClientConnection::new(write, read));
                let done = done.clone();
                pool.dispatch_locked(
                    Box::new(move || {
                        for request in connection.take().unwrap() {
                            let _ = request.respond(Response::from_string("transport-ready"));
                        }
                        done.send(id).unwrap();
                    }),
                    &mut queue,
                );
                clients.push(client);
            }
        }
        let last = clients.last_mut().unwrap();
        last.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        let request = format!("GET /api/health HTTP/1.1\r\nHost: {addr}\r\nConnection: keep-alive\r\n\r\n");
        let mut responses = Vec::new();
        let mut failure = None;
        for _ in 0..2 {
            if let Err(error) = last.write_all(request.as_bytes()) {
                failure = Some(error);
                break;
            }
            let mut body = Vec::new();
            while !body.ends_with(b"transport-ready") {
                let mut chunk = [0; 1024];
                match last.read(&mut chunk) {
                    Ok(0) => {
                        failure = Some(std::io::Error::other("connection closed before response"));
                        break;
                    }
                    Ok(n) => body.extend_from_slice(&chunk[..n]),
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
            responses.push(body);
        }
        for client in clients {
            let _ = client.shutdown(Shutdown::Both);
        }
        for _ in 0..MIN_THREADS + 1 {
            finishes.recv_timeout(Duration::from_secs(2)).unwrap();
        }
        assert!(
            failure.is_none(),
            "new connection stalled behind idle preconnections: {failure:?}"
        );
        assert_eq!(responses.len(), 2, "keepalive remains enabled and reusable");
        for response in responses {
            assert!(response.starts_with(b"HTTP/1.1 200"));
        }
    }
}
