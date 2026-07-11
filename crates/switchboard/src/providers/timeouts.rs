//! The four clocks a streaming LLM call needs.
//!
//! One `total` timeout is not enough. A 300s ceiling that also governs connect
//! means a dead upstream holds a connection for five minutes; a 2s ceiling that
//! also governs generation kills every long answer. They measure different things
//! and must be separate:
//!
//! - `connect` — the TCP/TLS handshake. A dead host should fail fast.
//! - `ttft`    — request sent, no first byte. The upstream accepted and went quiet.
//! - `stall`   — bytes started, then stopped. The classic half-dead stream.
//! - `total`   — the whole generation, however healthy it looks.
//!
//! Without `stall`, a provider that emits one token a minute forever is
//! indistinguishable from a fast one, right up to the `total` ceiling.

use super::{ProviderError, TimeoutKind};
use futures::stream::{BoxStream, StreamExt};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub connect: Duration,
    pub ttft: Duration,
    pub stall: Duration,
    pub total: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            connect: Duration::from_secs(2),
            ttft: Duration::from_secs(30),
            stall: Duration::from_secs(30),
            total: Duration::from_secs(300),
        }
    }
}

impl Timeouts {
    /// A client for non-streaming calls, where `total` can be enforced by reqwest.
    pub fn unary_client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(self.connect)
            .timeout(self.total)
            .build()
            .unwrap_or_default()
    }

    /// A client for streaming calls. `reqwest`'s request timeout would kill a
    /// perfectly healthy long generation, so only `connect` is enforced here; the
    /// other three clocks live in [`guard_stream`].
    pub fn stream_client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .connect_timeout(self.connect)
            .build()
            .unwrap_or_default()
    }
}

/// Wraps a provider event stream in the `ttft`, `stall`, and `total` clocks.
///
/// The first event is raced against `ttft`; every subsequent event against `stall`;
/// and the whole stream against `total`. Whichever fires first ends the stream with
/// the matching `Timeout` error, so the caller can tell "never started" from
/// "started then died".
pub fn guard_stream<T>(
    inner: BoxStream<'static, Result<T, ProviderError>>,
    timeouts: Timeouts,
) -> BoxStream<'static, Result<T, ProviderError>>
where
    T: Send + 'static,
{
    let started = Instant::now();

    Box::pin(async_stream::stream! {
        let mut inner = inner;
        let mut seen_first = false;

        loop {
            let remaining_total = match timeouts.total.checked_sub(started.elapsed()) {
                Some(d) if !d.is_zero() => d,
                _ => {
                    yield Err(ProviderError::Timeout(TimeoutKind::Total));
                    return;
                }
            };

            let idle_budget = if seen_first { timeouts.stall } else { timeouts.ttft };
            let idle_kind = if seen_first { TimeoutKind::Stall } else { TimeoutKind::Ttft };
            let budget = idle_budget.min(remaining_total);

            match tokio::time::timeout(budget, inner.next()).await {
                Err(_elapsed) => {
                    // Whichever clock was tighter is the one that fired.
                    let kind = if budget == remaining_total { TimeoutKind::Total } else { idle_kind };
                    yield Err(ProviderError::Timeout(kind));
                    return;
                }
                Ok(None) => return,
                Ok(Some(item)) => {
                    seen_first = true;
                    let is_err = item.is_err();
                    yield item;
                    if is_err {
                        return;
                    }
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn fast() -> Timeouts {
        Timeouts {
            connect: Duration::from_millis(10),
            ttft: Duration::from_millis(60),
            stall: Duration::from_millis(60),
            total: Duration::from_secs(10),
        }
    }

    #[tokio::test]
    async fn a_healthy_stream_passes_through_untouched() {
        let inner = stream::iter(vec![Ok(1u8), Ok(2), Ok(3)]).boxed();
        let out: Vec<_> = guard_stream(inner, fast()).collect().await;
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn a_silent_upstream_trips_ttft_not_stall() {
        let inner = stream::once(async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(1u8)
        })
        .boxed();

        let out: Vec<_> = guard_stream(inner, fast()).collect().await;
        assert!(matches!(
            out.as_slice(),
            [Err(ProviderError::Timeout(TimeoutKind::Ttft))]
        ));
    }

    #[tokio::test]
    async fn a_stream_that_dies_midway_trips_stall_not_ttft() {
        let inner = stream::once(async { Ok(1u8) })
            .chain(stream::once(async {
                tokio::time::sleep(Duration::from_millis(500)).await;
                Ok(2u8)
            }))
            .boxed();

        let out: Vec<_> = guard_stream(inner, fast()).collect().await;
        assert!(matches!(out.first(), Some(Ok(1))));
        assert!(
            matches!(
                out.get(1),
                Some(Err(ProviderError::Timeout(TimeoutKind::Stall)))
            ),
            "bytes arrived, then stopped: that is a stall, not a slow start"
        );
    }

    #[tokio::test]
    async fn total_wins_when_it_is_the_tighter_clock() {
        let timeouts = Timeouts {
            total: Duration::from_millis(40),
            ..fast()
        };
        let inner = stream::once(async {
            tokio::time::sleep(Duration::from_millis(500)).await;
            Ok(1u8)
        })
        .boxed();

        let out: Vec<_> = guard_stream(inner, timeouts).collect().await;
        assert!(matches!(
            out.as_slice(),
            [Err(ProviderError::Timeout(TimeoutKind::Total))]
        ));
    }
}
