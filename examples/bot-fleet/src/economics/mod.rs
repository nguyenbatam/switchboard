//! Was it worth it?
//!
//! A third party does not adopt a gateway because its architecture is tasteful. It adopts
//! it because the same answers cost less, or arrive faster, or because one wire format is
//! worth a small tax. This module measures which of those happened, in integers, and is
//! built to be able to return "none of them".

pub mod audit;
pub mod ledger;
pub mod money;
pub mod prices;
pub mod report;

pub use audit::{Finding, audit};
pub use ledger::{Answer, Ledger, Refusal, Tallies};
pub use money::{NANO_PER_USD, Nano};
pub use prices::{Price, PriceTable};
pub use report::{Report, Verdict};
