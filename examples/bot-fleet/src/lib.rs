//! A bot fleet, as a third party to switchboard.
//!
//! Twenty thousand bot accounts, twelve provider slots, and the architecture in between.
//! This crate is the worked example for `docs/16-example-bot-fleet.md`, which is the
//! concrete form of Shape B in `docs/14-workloads-and-clients.md`.
//!
//! **This crate does not depend on `switchboard`, and never may.** It reaches the gateway
//! over HTTP, exactly as any customer would. That constraint is what makes it evidence:
//! an example that could reach inside the thing it integrates with would be demonstrating
//! the opposite of what it teaches. See the comment at the top of the repository's root
//! `Cargo.toml`.
//!
//! The four modules answer four different questions:
//!
//! * [`contract`] — *what does any client owe the gateway?* Nothing here knows what a bot
//!   is. It is `docs/15 §7` and `§8` as code, and it is the part worth copying.
//! * [`fleet`] — *how do twenty thousand producers share twelve slots?* Broker, bounded
//!   consumer pool, coalescing, deadlines.
//! * [`platform`] — *where does the answer go?* The consumer sends the reply itself,
//!   which is the observation that makes the whole design legal.
//! * [`economics`] — *was any of this worth it?* The question a third party actually
//!   asks, answered in integers, by a report built to be able to say no.

pub mod contract;
pub mod economics;
pub mod fleet;
pub mod platform;
