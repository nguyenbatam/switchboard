//! Redis Streams: many producers, few consumers, bounded.
//!
//! `docs/14 §4` picks this deliberately. Kafka is the right *shape* — but you do not yet
//! need replay or independent consumer groups, and it is a broker, a rebalance protocol,
//! and an offset story you would have to operate. Redis is already deployed.
//!
//! The queue lives **at the edge**, not inside the gateway, because the edge is where the
//! deadline is known and where the reply channel lives. Twenty thousand bots publish and
//! forget; `N ≈ 16` consumers drain. `N` is a constant you choose, not twenty thousand,
//! and it does not move when the fleet grows to 200k.

use anyhow::{Context, Result};
use deadpool_redis::{Config as RedisConfig, Pool, Runtime};
use redis::AsyncCommands;
use redis::streams::{StreamReadOptions, StreamReadReply};

use super::envelope::Job;

/// What happened to a publish.
#[derive(Debug, PartialEq, Eq)]
pub enum Publish {
    Enqueued,
    /// The broker is full. **Answer the user now**, in the product: "Busy right now, try
    /// again shortly." `docs/14 §4`: people forgive a fast refusal; they do not forgive
    /// silence, and they have already left by the time a late answer arrives.
    Full,
}

pub struct Broker {
    pool: Pool,
    stream: String,
    group: String,
    /// The bound. Without it this is not a buffer, it is an unbounded queue with extra
    /// steps — and `docs/14 §2` is very clear that a queue does not create provider slots.
    max_len: usize,
}

impl Broker {
    pub async fn connect(url: &str, stream: &str, group: &str, max_len: usize) -> Result<Self> {
        let pool = RedisConfig::from_url(url)
            .create_pool(Some(Runtime::Tokio1))
            .context("building the redis pool")?;

        let broker = Self {
            pool,
            stream: stream.to_string(),
            group: group.to_string(),
            max_len,
        };
        broker.ensure_group().await?;
        Ok(broker)
    }

    async fn ensure_group(&self) -> Result<()> {
        let mut conn = self.pool.get().await?;
        // `MKSTREAM` so the first consumer can start before the first producer.
        let created: redis::RedisResult<String> = conn
            .xgroup_create_mkstream(self.stream.as_str(), self.group.as_str(), "0")
            .await;
        match created {
            Ok(_) => Ok(()),
            // BUSYGROUP: someone else created it. That is the normal case on restart.
            Err(e) if e.code() == Some("BUSYGROUP") => Ok(()),
            Err(e) => Err(e).context("creating the consumer group"),
        }
    }

    pub async fn len(&self) -> Result<usize> {
        let mut conn = self.pool.get().await?;
        Ok(conn.xlen(self.stream.as_str()).await?)
    }

    /// Fire and forget. The bot does not wait for an answer — it does not even wait to
    /// learn whether one will be produced.
    pub async fn publish(&self, job: &Job) -> Result<Publish> {
        let mut conn = self.pool.get().await?;

        // Admission control. Note honestly what this is and is not: `XLEN` then `XADD` is
        // not atomic, so under a thundering herd a few jobs slip past the bound. The
        // `MAXLEN ~` below is the real bound; this check is what lets us tell the *user*
        // "busy" instead of silently trimming their message from the stream later.
        let len: usize = conn.xlen(self.stream.as_str()).await?;
        if len >= self.max_len {
            return Ok(Publish::Full);
        }

        let payload = serde_json::to_string(job)?;
        let _: String = redis::cmd("XADD")
            .arg(self.stream.as_str())
            .arg("MAXLEN")
            .arg("~") // approximate trimming: O(1) amortised, and exactness buys nothing
            .arg(self.max_len)
            .arg("*")
            .arg("data")
            .arg(payload)
            .query_async(&mut conn)
            .await?;

        Ok(Publish::Enqueued)
    }

    /// Claim up to `count` jobs for this consumer. Blocks up to `block_ms`.
    pub async fn consume(
        &self,
        consumer: &str,
        count: usize,
        block_ms: usize,
    ) -> Result<Vec<Delivery>> {
        let mut conn = self.pool.get().await?;

        let opts = StreamReadOptions::default()
            .group(self.group.as_str(), consumer)
            .count(count)
            .block(block_ms);

        let reply: StreamReadReply = conn
            .xread_options(&[self.stream.as_str()], &[">"], &opts)
            .await?;

        let mut out = Vec::new();
        for key in reply.keys {
            for entry in key.ids {
                let Some(raw) = entry.get::<String>("data") else {
                    // A malformed entry would otherwise be redelivered forever. Ack it and
                    // move on; a real fleet would copy it to a dead-letter stream first.
                    self.ack(&entry.id).await.ok();
                    continue;
                };
                match serde_json::from_str::<Job>(&raw) {
                    Ok(job) => out.push(Delivery { id: entry.id, job }),
                    Err(e) => {
                        tracing::warn!(id = %entry.id, error = %e, "undeserialisable job, acking");
                        self.ack(&entry.id).await.ok();
                    }
                }
            }
        }
        Ok(out)
    }

    /// Acknowledge. Until this happens the entry sits in the group's pending list, which
    /// is what makes a dead consumer's work recoverable — the Redis Streams analogue of
    /// `docs/14 §5`'s "lease, not lock".
    pub async fn ack(&self, id: &str) -> Result<()> {
        let mut conn = self.pool.get().await?;
        let _: i64 = conn
            .xack(self.stream.as_str(), self.group.as_str(), &[id])
            .await?;
        Ok(())
    }
}

#[derive(Debug)]
pub struct Delivery {
    pub id: String,
    pub job: Job,
}
