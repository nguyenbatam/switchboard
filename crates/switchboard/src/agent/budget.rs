use std::time::{Duration, Instant};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum BudgetError {
    #[error("max iterations ({max}) exceeded")]
    MaxIterations { max: u32 },
    #[error("max total tokens ({max}) exceeded")]
    MaxTokens { max: u32 },
    #[error("deadline exceeded")]
    Deadline,
}

#[derive(Debug, Clone)]
pub struct Budget {
    max_iterations: u32,
    max_total_tokens: u32,
    deadline: Instant,
    iterations: u32,
    total_tokens: u32,
}

impl Budget {
    pub fn new(max_iterations: u32, max_total_tokens: u32, request_timeout: Duration) -> Self {
        Self {
            max_iterations,
            max_total_tokens,
            deadline: Instant::now() + request_timeout,
            iterations: 0,
            total_tokens: 0,
        }
    }

    pub fn tick(&mut self) -> Result<(), BudgetError> {
        if Instant::now() >= self.deadline {
            return Err(BudgetError::Deadline);
        }
        self.iterations += 1;
        if self.iterations > self.max_iterations {
            return Err(BudgetError::MaxIterations {
                max: self.max_iterations,
            });
        }
        Ok(())
    }

    pub fn add_tokens(&mut self, n: u32) -> Result<(), BudgetError> {
        self.total_tokens = self.total_tokens.saturating_add(n);
        if self.total_tokens > self.max_total_tokens {
            return Err(BudgetError::MaxTokens {
                max: self.max_total_tokens,
            });
        }
        Ok(())
    }

    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    pub fn iterations(&self) -> u32 {
        self.iterations
    }
}
