const FIXED_METHOD_OPERATORS: &[&str] = &["+", "-", "*", "/", "%", "^", "<", "<=", ">", ">="];

/// Whether `operator` is a custom operator token rather than an existing
/// reserved or fixed token.
pub fn is_custom_operator_token(operator: &str) -> bool {
    let Some((&first, rest)) = operator.as_bytes().split_first() else {
        return false;
    };

    first != b'='
        && is_custom_operator_byte(first)
        && rest.iter().copied().all(is_custom_operator_byte)
        && !is_reserved_or_fixed_operator(operator)
}

/// Whether `operator` can name a type-carried method member.
pub fn is_method_operator(operator: &str) -> bool {
    FIXED_METHOD_OPERATORS.contains(&operator) || is_custom_operator_token(operator)
}

pub(crate) fn is_custom_operator_byte(byte: u8) -> bool {
    matches!(
        byte,
        b'+' | b'-' | b'*' | b'/' | b'%' | b'^' | b'&' | b'<' | b'>' | b'!' | b'~' | b'$' | b'='
    )
}

/// Whether `operator` is an existing fixed or syntax-reserved token.
pub fn is_reserved_or_fixed_operator(operator: &str) -> bool {
    FIXED_METHOD_OPERATORS.contains(&operator)
        || matches!(
            operator,
            "=" | "=>"
                | "=="
                | "!="
                | "->"
                | ":"
                | "::"
                | ":="
                | ":.."
                | "."
                | ".."
                | "?"
                | "?."
                | "??"
                | "?^"
                | "?!"
                | "?>"
                | "!"
                | "@"
                | "|"
                | "|>"
                | "||"
                | "&&"
        )
}

#[cfg(test)]
mod tests {
    use super::{is_custom_operator_token, is_method_operator};

    #[test]
    fn recognizes_custom_operator_token_grammar() {
        for operator in ["**", "$$", "&", "!!", "<=>", "+~="] {
            assert!(is_custom_operator_token(operator), "{operator}");
        }
        for operator in ["", "=+", "*|", "⊗", "+:", "+?"] {
            assert!(!is_custom_operator_token(operator), "{operator}");
        }
    }

    #[test]
    fn method_operators_include_fixed_and_custom_but_not_structural_tokens() {
        for operator in ["+", "<=", ">=", "**", "$$", "&"] {
            assert!(is_method_operator(operator), "{operator}");
        }
        for operator in [
            "=", "=>", "==", "!=", "->", ":", "::", ":=", ":..", ".", "..", "?", "?.", "??", "?^",
            "?!", "?>", "!", "@", "|", "|>", "||", "&&",
        ] {
            assert!(!is_method_operator(operator), "{operator}");
        }
    }
}
