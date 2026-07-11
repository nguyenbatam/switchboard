//! Where the answer actually goes.
//!
//! This module is the whole reason Shape B works. `docs/14 §4`:
//!
//! > The reply does not have to travel back over the request's connection. The bot
//! > already holds a channel to the user — a Telegram token, a Discord webhook. The
//! > consumer of the answer is durable. Everything asynchronous becomes legal.
//!
//! The consumer sends the reply itself, using the right bot's credentials. **No
//! reply-routing exists to get wrong**, because the thing that called the gateway is the
//! same thing that talks to the user.
//!
//! A real implementation is a `POST` to `api.telegram.org/bot<token>/sendMessage`. It is
//! omitted because it teaches nothing about switchboard, and because a mock lets the
//! simulator in `bin/` run against nothing but a gateway and a Redis.

use std::sync::atomic::{AtomicU64, Ordering};

/// What a bot says when it cannot get an answer in time.
///
/// `docs/14 §4`: "Degrade in the product, not in the infrastructure." A fast refusal is
/// forgiven; silence is not, and by the time a late answer arrives the user has left.
pub const BUSY_REPLY: &str = "Busy right now — try again in a moment.";

pub trait Platform: Send + Sync {
    /// Deliver `text` to `conversation_id` using `bot_id`'s credentials.
    fn send(&self, bot_id: &str, conversation_id: &str, text: &str);

    fn send_busy(&self, bot_id: &str, conversation_id: &str) {
        self.send(bot_id, conversation_id, BUSY_REPLY);
    }
}

/// Counts instead of delivering. Enough to run the simulator and read the numbers that
/// `docs/14 §9` says actually matter.
#[derive(Default)]
pub struct MockPlatform {
    answered: AtomicU64,
    busy: AtomicU64,
}

impl MockPlatform {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn answered(&self) -> u64 {
        self.answered.load(Ordering::Relaxed)
    }

    pub fn busy(&self) -> u64 {
        self.busy.load(Ordering::Relaxed)
    }
}

impl Platform for MockPlatform {
    fn send(&self, bot_id: &str, conversation_id: &str, text: &str) {
        if text == BUSY_REPLY {
            self.busy.fetch_add(1, Ordering::Relaxed);
        } else {
            self.answered.fetch_add(1, Ordering::Relaxed);
        }
        tracing::debug!(bot = %bot_id, conversation = %conversation_id, "reply sent");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_replies_are_counted_separately_from_answers() {
        let p = MockPlatform::new();
        p.send("bot", "conv", "a real answer");
        p.send_busy("bot", "conv");
        assert_eq!(p.answered(), 1);
        assert_eq!(p.busy(), 1);
    }
}
