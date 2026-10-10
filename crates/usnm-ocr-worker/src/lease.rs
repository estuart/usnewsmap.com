//! Keeping a message hidden while its page is worked on.
//!
//! A received message is hidden for the visibility timeout (600 s). While
//! the page is fetched and read, a background task renews it every
//! `every` (180 s), so a slow page keeps its lease and a crashed worker's
//! page comes back within ten minutes. Each renewal returns a new pop
//! receipt, which the delete (or release) at the end must use.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{watch, Mutex};

use crate::queue::{LeaseLost, Queue, Received};

/// A held message and its current pop receipt.
#[derive(Debug)]
pub struct Lease {
    queue: Arc<dyn Queue>,
    msg: Mutex<Received>,
    lost: watch::Sender<bool>,
}

impl Lease {
    pub fn new(queue: Arc<dyn Queue>, msg: Received) -> Arc<Self> {
        Arc::new(Self {
            queue,
            msg: Mutex::new(msg),
            lost: watch::channel(false).0,
        })
    }

    /// Whether a renewal found the message gone or taken.
    pub fn is_lost(&self) -> bool {
        *self.lost.borrow()
    }

    /// Hide the message for `visibility` from now, keeping the new receipt.
    pub async fn renew(&self, visibility: Duration) -> anyhow::Result<()> {
        let mut msg = self.msg.lock().await;
        match self.queue.update(&msg, visibility).await {
            Ok(receipt) => {
                msg.pop_receipt = receipt;
                Ok(())
            }
            Err(e) => {
                if e.downcast_ref::<LeaseLost>().is_some() {
                    self.lost.send_replace(true);
                }
                Err(e)
            }
        }
    }

    /// Renew every `every` until the returned guard is dropped.
    pub fn keep_alive(self: &Arc<Self>, every: Duration, visibility: Duration) -> Renewer {
        let lease = self.clone();
        let task = tokio::spawn(async move {
            let mut tick = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = lease.renew(visibility).await {
                    let id = lease.msg.lock().await.id.clone();
                    if lease.is_lost() {
                        tracing::warn!(message_id = %id, "lease lost; the page may be done twice");
                        return;
                    }
                    tracing::warn!(message_id = %id, error = %e, "lease renewal failed; retrying");
                }
            }
        });
        Renewer(task)
    }

    /// Delete the message with its current receipt.
    pub async fn delete(&self) -> anyhow::Result<()> {
        let msg = self.msg.lock().await;
        self.queue.delete(&msg).await
    }

    pub async fn message(&self) -> Received {
        self.msg.lock().await.clone()
    }
}

/// Stops the renewal task when dropped.
#[derive(Debug)]
pub struct Renewer(tokio::task::JoinHandle<()>);

impl Drop for Renewer {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;

    /// Records updates; each gives receipt `r<n>`. Updates after `lose_after` fail as lost.
    #[derive(Debug, Default)]
    struct Mock {
        updates: StdMutex<Vec<(String, Duration)>>,
        deleted: StdMutex<Vec<String>>,
        lose_after: Option<usize>,
    }

    #[async_trait]
    impl Queue for Mock {
        async fn receive(&self, _: &str, _: Duration) -> anyhow::Result<Option<Received>> {
            Ok(None)
        }
        async fn update(&self, msg: &Received, v: Duration) -> anyhow::Result<String> {
            let mut u = self.updates.lock().unwrap();
            if self.lose_after.is_some_and(|n| u.len() >= n) {
                return Err(LeaseLost(msg.id.clone()).into());
            }
            u.push((msg.pop_receipt.clone(), v));
            Ok(format!("r{}", u.len()))
        }
        async fn delete(&self, msg: &Received) -> anyhow::Result<()> {
            self.deleted.lock().unwrap().push(msg.pop_receipt.clone());
            Ok(())
        }
        async fn send(&self, _: &str, _: &str) -> anyhow::Result<()> {
            Ok(())
        }
    }

    fn received() -> Received {
        Received {
            queue: "ocr-bulk".into(),
            id: "m1".into(),
            pop_receipt: "r0".into(),
            dequeue_count: 1,
            text: String::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn renews_on_schedule_and_deletes_with_the_latest_receipt() {
        let mock = Arc::new(Mock::default());
        let lease = Lease::new(mock.clone(), received());
        let every = Duration::from_secs(180);
        let guard = lease.keep_alive(every, Duration::from_secs(600));
        tokio::time::sleep(Duration::from_secs(179)).await;
        assert!(mock.updates.lock().unwrap().is_empty());
        tokio::time::sleep(Duration::from_secs(221)).await; // t = 400: two renewals
        assert_eq!(
            *mock.updates.lock().unwrap(),
            [
                ("r0".to_owned(), Duration::from_secs(600)),
                ("r1".to_owned(), Duration::from_secs(600))
            ]
        );
        drop(guard);
        tokio::time::sleep(Duration::from_secs(1000)).await;
        assert_eq!(
            mock.updates.lock().unwrap().len(),
            2,
            "stopped with its guard"
        );
        lease.delete().await.unwrap();
        assert_eq!(*mock.deleted.lock().unwrap(), ["r2"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_lease_stops_renewing() {
        let mock = Arc::new(Mock {
            lose_after: Some(1),
            ..Mock::default()
        });
        let lease = Lease::new(mock.clone(), received());
        let _guard = lease.keep_alive(Duration::from_secs(180), Duration::from_secs(600));
        tokio::time::sleep(Duration::from_secs(1000)).await;
        assert!(lease.is_lost());
        assert_eq!(mock.updates.lock().unwrap().len(), 1);
    }
}
