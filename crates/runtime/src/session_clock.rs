use std::future::pending;
use std::pin::Pin;
use std::time::Duration;

use tokio::time::{Instant, Sleep};

#[derive(Clone, Debug)]
pub struct SessionClock {
    origin: Instant,
}

impl SessionClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }

    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis()).unwrap_or(u64::MAX)
    }
}

impl Default for SessionClock {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug)]
pub struct TurnFinalizationDeadline {
    sleep: Option<Pin<Box<Sleep>>>,
}

impl TurnFinalizationDeadline {
    pub const fn new() -> Self {
        Self { sleep: None }
    }

    pub fn arm_after(&mut self, duration: Duration) {
        self.sleep = Some(Box::pin(tokio::time::sleep(duration)));
    }

    pub fn disarm(&mut self) {
        self.sleep = None;
    }

    pub async fn wait(&mut self) {
        match self.sleep.as_mut() {
            Some(sleep) => sleep.as_mut().await,
            None => pending().await,
        }
        self.sleep = None;
    }
}

impl Default for TurnFinalizationDeadline {
    fn default() -> Self {
        Self::new()
    }
}
