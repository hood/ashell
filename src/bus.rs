//! Global "something changed" signal: async work and watchers bump it,
//! follow-up tasks wait on it and re-render.

use std::sync::{Condvar, Mutex};
use std::time::Duration;

pub struct Bus {
    generation: Mutex<u64>,
    cv: Condvar,
}

pub static BUS: Bus = Bus { generation: Mutex::new(0), cv: Condvar::new() };

impl Bus {
    pub fn bump(&self) {
        *self.generation.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    pub fn current(&self) -> u64 {
        *self.generation.lock().unwrap()
    }

    /// Waits until the generation moves past `seen` or `timeout` elapses.
    pub fn wait_past(&self, seen: u64, timeout: Duration) -> u64 {
        let g = self.generation.lock().unwrap();
        let (g, _) = self.cv.wait_timeout_while(g, timeout, |g| *g == seen).unwrap();
        *g
    }
}
