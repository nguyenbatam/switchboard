//! Shape B: the fan-in architecture.
//!
//! The problem was never the twenty thousand bots. It is that they are twenty thousand
//! *uncoordinated producers* pointed at twelve provider slots. Each one, alone, is
//! reasonable. Together they are a denial-of-service with a support contract.

pub mod broker;
pub mod coalesce;
pub mod consumer;
pub mod envelope;

pub use broker::{Broker, Delivery, Publish};
pub use coalesce::{CoalesceGuard, Coalescer};
pub use consumer::{ConsumerPool, Solvency};
pub use envelope::{Deadline, Job, now_ms};
