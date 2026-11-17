//! A deliberately small scanner over shell source.
//!
//! It answers two questions the supervisor needs before it runs a command:
//! where the command substitutions are, and whether an expansion is unquoted.
//! It is not a shell lexer: it tracks quotes and escapes, which is enough to
//! locate `$( )` and backticks. Statements containing heredocs are excluded by
//! the caller, because a heredoc body would need real parsing.

/// One command substitution, located in the source by character index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Substitution {
    /// Character index of the `$(` or backtick.
    pub start: usize,
    /// Character index just past the closing `)` or backtick.
    pub end: usize,
    /// The inner script, without the delimiters.
    pub inner: String,
    /// Whether the substitution sits inside a double-quoted region.
    pub quoted: bool,
}

/// Finds `$( ... )` and `` `...` `` substitutions, skipping single-quoted text.
pub fn substitutions(src: &str) -> Vec<Substitution> {
    let chars: Vec<char> = src.chars().collect();
    let mut found = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];

        if in_single {
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        match c {
            '\\' => {
                i += 2;
                continue;
            }
            '\'' if !in_double => {
                in_single = true;
                i += 1;
                continue;
            }
            '"' => {
                in_double = !in_double;
                i += 1;
                continue;
            }
            '`' => {
                if let Some(end) = find_backtick_end(&chars, i + 1) {
                    let inner: String = chars[i + 1..end].iter().collect();
                    found.push(Substitution {
                        start: i,
                        end: end + 1,
                        inner,
                        quoted: in_double,
                    });
                    i = end + 1;
                    continue;
                }
                i += 1;
                continue;
            }
            '$' if i + 1 < chars.len() && chars[i + 1] == '(' => {
                // `$((` is arithmetic expansion, not a command substitution.
                if i + 2 < chars.len() && chars[i + 2] == '(' {
                    i += 3;
                    continue;
                }
                if let Some(end) = find_paren_end(&chars, i + 2) {
                    let inner: String = chars[i + 2..end].iter().collect();
                    found.push(Substitution {
                        start: i,
                        end: end + 1,
                        inner,
                        quoted: in_double,
                    });
                    i = end + 1;
                    continue;
                }
                i += 1;
                continue;
            }
            _ => {}
        }

        i += 1;
    }

    found
}

fn find_backtick_end(chars: &[char], from: usize) -> Option<usize> {
    let mut i = from;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 2,
            '`' => return Some(i),
            _ => i += 1,
        }
    }
    None
}

fn find_paren_end(chars: &[char], from: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = from;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        let c = chars[i];

        if in_single {
            if c == '\'' {
                in_single = false;
            }
            i += 1;
            continue;
        }

        match c {
            '\\' => {
                i += 2;
                continue;
            }
            '\'' if !in_double => in_single = true,
            '"' => in_double = !in_double,
            '(' if !in_double => depth += 1,
            ')' if !in_double => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_a_substitution() {
        let subs = substitutions("echo $(date +%s) done");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].inner, "date +%s");
        assert!(!subs[0].quoted);
    }

    #[test]
    fn ignores_single_quotes_and_arithmetic() {
        assert!(substitutions("echo '$(date)'").is_empty());
        assert!(substitutions("echo $((1 + 2))").is_empty());
    }

    #[test]
    fn handles_nesting_and_quotes() {
        let subs = substitutions(r#"echo "$(echo "$(date)")" x"#);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].inner, r#"echo "$(date)""#);
        assert!(subs[0].quoted);
    }

    #[test]
    fn finds_backticks() {
        let subs = substitutions("echo `date`");
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].inner, "date");
    }
}
