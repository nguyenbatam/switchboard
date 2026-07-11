use crate::agent::registry::ToolSpec;
use crate::agent::taint::{Effects, Trust};
use crate::domain::schema::{FieldSpec, ToolParameters};
use crate::domain::values::ParsedToolArguments;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::str::Chars;
use std::time::Duration;

#[derive(Deserialize)]
struct CalculatorArgs {
    expression: String,
}

pub fn build() -> ToolSpec {
    ToolSpec::new(
        "calculator",
        "Evaluate a basic arithmetic expression (+, -, *, /, parentheses).",
        ToolParameters::object(
            BTreeMap::from([(
                "expression".into(),
                FieldSpec::string(r#"Arithmetic expression, e.g. "(2 + 3) * 4""#),
            )]),
            vec!["expression".into()],
            false,
        ),
        Trust::Trusted,
        Effects::Pure,
        Duration::from_secs(1),
        |_cx, args| run(args),
    )
}

fn run(args: &ParsedToolArguments) -> Result<String, String> {
    let args: CalculatorArgs = args.typed()?;
    let value = evaluate(&args.expression).map_err(|e| e.to_string())?;
    Ok(value.to_string())
}

#[derive(Debug)]
enum CalcError {
    Parse(&'static str),
    DivZero,
}

impl std::fmt::Display for CalcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(msg) => write!(f, "{msg}"),
            Self::DivZero => write!(f, "division by zero"),
        }
    }
}

fn evaluate(input: &str) -> Result<f64, CalcError> {
    let mut parser = Parser {
        chars: input.chars().peekable(),
    };
    let value = parser.expr()?;
    parser.skip_ws();
    if parser.chars.peek().is_some() {
        return Err(CalcError::Parse("unexpected trailing characters"));
    }
    if !value.is_finite() {
        return Err(CalcError::DivZero);
    }
    Ok(value)
}

struct Parser<'a> {
    chars: std::iter::Peekable<Chars<'a>>,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while matches!(self.chars.peek(), Some(c) if c.is_whitespace()) {
            self.chars.next();
        }
    }

    fn expr(&mut self) -> Result<f64, CalcError> {
        let mut value = self.term()?;
        loop {
            self.skip_ws();
            match self.chars.peek().copied() {
                Some('+') => {
                    self.chars.next();
                    value += self.term()?;
                }
                Some('-') => {
                    self.chars.next();
                    value -= self.term()?;
                }
                _ => break,
            }
        }
        Ok(value)
    }

    fn term(&mut self) -> Result<f64, CalcError> {
        let mut value = self.factor()?;
        loop {
            self.skip_ws();
            match self.chars.peek().copied() {
                Some('*') => {
                    self.chars.next();
                    value *= self.factor()?;
                }
                Some('/') => {
                    self.chars.next();
                    let rhs = self.factor()?;
                    if rhs == 0.0 {
                        return Err(CalcError::DivZero);
                    }
                    value /= rhs;
                }
                _ => break,
            }
        }
        Ok(value)
    }

    fn factor(&mut self) -> Result<f64, CalcError> {
        self.skip_ws();
        match self.chars.peek().copied() {
            Some('(') => {
                self.chars.next();
                let value = self.expr()?;
                self.skip_ws();
                if self.chars.next() != Some(')') {
                    return Err(CalcError::Parse("expected ')'"));
                }
                Ok(value)
            }
            Some('+') => {
                self.chars.next();
                self.factor()
            }
            Some('-') => {
                self.chars.next();
                Ok(-self.factor()?)
            }
            Some(c) if c.is_ascii_digit() || c == '.' => Ok(self.number()?),
            _ => Err(CalcError::Parse("expected number or '('")),
        }
    }

    fn number(&mut self) -> Result<f64, CalcError> {
        self.skip_ws();
        let start = self.chars.clone();
        let mut s = String::new();
        let mut saw_dot = false;
        while let Some(c) = self.chars.peek().copied() {
            if c.is_ascii_digit() {
                s.push(c);
                self.chars.next();
            } else if c == '.' && !saw_dot {
                saw_dot = true;
                s.push(c);
                self.chars.next();
            } else {
                break;
            }
        }
        if s.is_empty() || s == "." {
            let _ = start;
            return Err(CalcError::Parse("invalid number"));
        }
        s.parse::<f64>()
            .map_err(|_| CalcError::Parse("invalid number"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_arithmetic() {
        assert_eq!(evaluate("2+3*4").unwrap(), 14.0);
        assert_eq!(evaluate("(2+3)*4").unwrap(), 20.0);
        assert_eq!(evaluate("10/4").unwrap(), 2.5);
    }

    #[test]
    fn division_by_zero() {
        assert!(evaluate("1/0").is_err());
    }
}
