use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use aven_core::Span;

use crate::is_custom_operator_token;

/// One of the fixed precedence levels available to custom operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorPrecedence {
    Union,
    Pipe,
    Coalesce,
    Or,
    And,
    Comparison,
    Additive,
    Multiplicative,
    Exponentiation,
}

impl OperatorPrecedence {
    pub const fn anchor(self) -> &'static str {
        match self {
            Self::Union => "|",
            Self::Pipe => "|>",
            Self::Coalesce => "??",
            Self::Or => "||",
            Self::And => "&&",
            Self::Comparison => "==",
            Self::Additive => "+",
            Self::Multiplicative => "*",
            Self::Exponentiation => "^",
        }
    }

    pub fn from_anchor(anchor: &str) -> Option<Self> {
        match anchor {
            "|" => Some(Self::Union),
            "|>" => Some(Self::Pipe),
            "??" => Some(Self::Coalesce),
            "||" => Some(Self::Or),
            "&&" => Some(Self::And),
            "==" => Some(Self::Comparison),
            "+" => Some(Self::Additive),
            "*" => Some(Self::Multiplicative),
            "^" => Some(Self::Exponentiation),
            _ => None,
        }
    }

    pub(crate) const fn infix_binding_power(
        self,
        associativity: OperatorAssociativity,
    ) -> (u8, u8) {
        let precedence = match self {
            Self::Union => 1,
            Self::Pipe => 2,
            Self::Coalesce => 3,
            Self::Or => 4,
            Self::And => 5,
            Self::Comparison => 6,
            Self::Additive => 7,
            Self::Multiplicative => 8,
            Self::Exponentiation => 9,
        };
        let right_binding_power = match associativity {
            OperatorAssociativity::Right => precedence,
            // `None` uses the left-shaped power for recovery. Slice 3 will
            // reject same-level chaining before accepting that grouping.
            OperatorAssociativity::Left | OperatorAssociativity::None => precedence + 1,
        };

        (precedence, right_binding_power)
    }
}

impl fmt::Display for OperatorPrecedence {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.anchor())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OperatorAssociativity {
    Left,
    Right,
    None,
}

impl OperatorAssociativity {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Left => "left",
            Self::Right => "right",
            Self::None => "none",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "left" => Some(Self::Left),
            "right" => Some(Self::Right),
            "none" => Some(Self::None),
            _ => None,
        }
    }
}

impl fmt::Display for OperatorAssociativity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.name())
    }
}

/// The authority and source location that supplied an operator declaration.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OperatorOrigin {
    Manifest {
        path: PathBuf,
        span: Span,
    },
    Shebang {
        span: Span,
    },
    Argv {
        declaration_index: usize,
        span: Span,
    },
    Platform {
        registration_index: usize,
    },
}

impl fmt::Display for OperatorOrigin {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Manifest { path, span } => write!(
                formatter,
                "manifest `{}` at bytes {}..{}",
                path.display(),
                span.start,
                span.end
            ),
            Self::Shebang { span } => write!(
                formatter,
                "the entry's first-line shebang at bytes {}..{}",
                span.start, span.end
            ),
            Self::Argv {
                declaration_index,
                span,
            } => write!(
                formatter,
                "command-line operator declaration {} at bytes {}..{}",
                declaration_index + 1,
                span.start,
                span.end
            ),
            Self::Platform { registration_index } => {
                write!(
                    formatter,
                    "platform registration {}",
                    registration_index + 1
                )
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OperatorFixity {
    precedence: OperatorPrecedence,
    associativity: OperatorAssociativity,
    origin: OperatorOrigin,
}

impl OperatorFixity {
    pub fn new(
        precedence: OperatorPrecedence,
        associativity: OperatorAssociativity,
        origin: OperatorOrigin,
    ) -> Self {
        Self {
            precedence,
            associativity,
            origin,
        }
    }

    pub const fn precedence(&self) -> OperatorPrecedence {
        self.precedence
    }

    pub const fn associativity(&self) -> OperatorAssociativity {
        self.associativity
    }

    pub const fn origin(&self) -> &OperatorOrigin {
        &self.origin
    }
}

impl fmt::Display for OperatorFixity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "precedence `{}`, associativity `{}`",
            self.precedence, self.associativity
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OperatorFixityTableError {
    InvalidToken {
        token: String,
        fixity: OperatorFixity,
    },
    Duplicate {
        token: String,
        first: OperatorFixity,
        second: OperatorFixity,
    },
}

/// A validated, immutable mapping from custom tokens to their fixities.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorFixityTable {
    entries: BTreeMap<String, OperatorFixity>,
    fingerprint: String,
}

impl Default for OperatorFixityTable {
    fn default() -> Self {
        let entries = BTreeMap::new();
        let fingerprint = normalized_fingerprint(&entries);
        Self {
            entries,
            fingerprint,
        }
    }
}

impl OperatorFixityTable {
    pub fn try_from_entries(
        entries: impl IntoIterator<Item = (String, OperatorFixity)>,
    ) -> Result<Self, OperatorFixityTableError> {
        let mut validated = BTreeMap::new();

        for (token, fixity) in entries {
            if !is_custom_operator_token(&token) {
                return Err(OperatorFixityTableError::InvalidToken { token, fixity });
            }
            if let Some(first) = validated.insert(token.clone(), fixity.clone()) {
                return Err(OperatorFixityTableError::Duplicate {
                    token,
                    first,
                    second: fixity,
                });
            }
        }

        let fingerprint = normalized_fingerprint(&validated);
        Ok(Self {
            entries: validated,
            fingerprint,
        })
    }

    pub fn get(&self, token: &str) -> Option<&OperatorFixity> {
        self.entries.get(token)
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = (&str, &OperatorFixity)> {
        self.entries
            .iter()
            .map(|(token, fixity)| (token.as_str(), fixity))
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// A stable, order-independent serialization of the syntax-relevant data.
    ///
    /// Origins are deliberately omitted: changing only where an identical
    /// declaration came from cannot change parsing.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn normalized_fingerprint(entries: &BTreeMap<String, OperatorFixity>) -> String {
    let mut fingerprint = String::from("aven-operator-fixities-v1\n");
    for (token, fixity) in entries {
        fingerprint.push_str(token);
        fingerprint.push(':');
        fingerprint.push_str(fixity.precedence.anchor());
        fingerprint.push(':');
        fingerprint.push_str(fixity.associativity.name());
        fingerprint.push('\n');
    }
    fingerprint
}

#[cfg(test)]
mod tests {
    use super::{
        OperatorAssociativity, OperatorFixity, OperatorFixityTable, OperatorOrigin,
        OperatorPrecedence,
    };

    const PRECEDENCES: [OperatorPrecedence; 9] = [
        OperatorPrecedence::Union,
        OperatorPrecedence::Pipe,
        OperatorPrecedence::Coalesce,
        OperatorPrecedence::Or,
        OperatorPrecedence::And,
        OperatorPrecedence::Comparison,
        OperatorPrecedence::Additive,
        OperatorPrecedence::Multiplicative,
        OperatorPrecedence::Exponentiation,
    ];

    #[test]
    fn every_precedence_anchor_round_trips() {
        for precedence in PRECEDENCES {
            assert_eq!(
                OperatorPrecedence::from_anchor(precedence.anchor()),
                Some(precedence)
            );
        }
    }

    #[test]
    fn precedence_maps_to_existing_internal_binding_powers() {
        for (index, precedence) in PRECEDENCES.into_iter().enumerate() {
            let power = u8::try_from(index + 1).expect("nine precedence levels fit in u8");
            assert_eq!(
                precedence.infix_binding_power(OperatorAssociativity::Left),
                (power, power + 1)
            );
            assert_eq!(
                precedence.infix_binding_power(OperatorAssociativity::Right),
                (power, power)
            );
        }
    }

    #[test]
    fn fingerprint_is_stable_and_order_independent() {
        let first = OperatorFixityTable::try_from_entries([
            declaration("$$", OperatorPrecedence::Multiplicative, 0),
            declaration("**", OperatorPrecedence::Exponentiation, 1),
        ])
        .expect("test declarations are valid and distinct");
        let second = OperatorFixityTable::try_from_entries([
            declaration("**", OperatorPrecedence::Exponentiation, 8),
            declaration("$$", OperatorPrecedence::Multiplicative, 9),
        ])
        .expect("test declarations are valid and distinct");

        assert_eq!(first.fingerprint(), second.fingerprint());
        assert_eq!(
            first.fingerprint(),
            "aven-operator-fixities-v1\n$$:*:left\n**:^:left\n"
        );
    }

    fn declaration(
        token: &str,
        precedence: OperatorPrecedence,
        registration_index: usize,
    ) -> (String, OperatorFixity) {
        (
            token.to_owned(),
            OperatorFixity::new(
                precedence,
                OperatorAssociativity::Left,
                OperatorOrigin::Platform { registration_index },
            ),
        )
    }
}
