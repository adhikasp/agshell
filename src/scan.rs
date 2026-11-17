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

/// Another interpreter's script, handed to it as a quoted word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForeignScript {
    /// The interpreter the script was meant for: `pwsh` or `powershell`.
    pub interpreter: String,
    /// The quoted argument, quotes included, exactly as written.
    pub word: String,
}

/// Programs that run a script handed to them as text, so a `$…` inside that text
/// belongs to *them*, not to the shell that is about to expand it.
const SCRIPT_INTERPRETERS: &[&str] = &["pwsh", "powershell"];

/// Finds `pwsh … -Command "… $… …"`.
///
/// bash expands `$…` and backticks inside double quotes before the interpreter
/// ever sees the script, so PowerShell's own `$_`, `$env:X` and `$( )` arrive
/// mangled or empty — as a `ParserError` pointing at nonsense. The fix is to
/// single-quote the argument.
#[must_use]
pub fn foreign_script_expansion(src: &str) -> Option<ForeignScript> {
    let mut words = split_words(src);
    while words
        .first()
        .is_some_and(|w| matches!(w.as_str(), "&" | "command"))
    {
        words.remove(0);
    }
    let interpreter = script_interpreter(words.first()?)?;

    for (i, word) in words.iter().enumerate().skip(1) {
        let flag = word.trim_start_matches(['-', '/']).to_ascii_lowercase();
        if !matches!(flag.as_str(), "command" | "c" | "commandwithargs") {
            continue;
        }
        let Some(argument) = words.get(i + 1) else {
            continue;
        };
        if argument.starts_with('"') && (argument.contains('$') || argument.contains('`')) {
            return Some(ForeignScript {
                interpreter,
                word: argument.clone(),
            });
        }
    }

    None
}

/// The interpreter's bare name, or `None` if this word is not one of ours.
fn script_interpreter(word: &str) -> Option<String> {
    let name = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .trim_matches('"')
        .to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(name.as_str());
    SCRIPT_INTERPRETERS
        .contains(&name)
        .then(|| name.to_string())
}

/// Splits source into words at unquoted whitespace, keeping the quotes in the
/// word text. Only word boundaries matter here, not expansion.
fn split_words(src: &str) -> Vec<String> {
    let chars: Vec<char> = src.chars().collect();
    let mut words = Vec::new();
    let mut i = 0;
    let mut in_single = false;
    let mut in_double = false;

    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() && !in_single && !in_double {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }

        let start = i;
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
                '\\' => i = (i + 2).min(chars.len()),
                '\'' if !in_double => {
                    in_single = true;
                    i += 1;
                }
                '"' => {
                    in_double = !in_double;
                    i += 1;
                }
                c if c.is_whitespace() && !in_double => break,
                _ => i += 1,
            }
        }

        words.push(chars[start..i].iter().collect());
        if i == start {
            // Never stall on a character the scan above refused to advance past.
            i += 1;
        }
    }

    words
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

    #[test]
    fn finds_a_script_bash_would_expand_first() {
        let hit = foreign_script_expansion(
            r#"pwsh -NoProfile -Command "Get-CimInstance X | Select-Object @{n='GB';e={$_.Capacity/1GB}}""#,
        )
        .expect("a hit");
        assert_eq!(hit.interpreter, "pwsh");
        assert!(hit.word.contains("$_"));
    }

    #[test]
    fn accepts_single_quoted_and_file_scripts() {
        assert!(foreign_script_expansion(r#"pwsh -Command 'Get-Item $_.Name'"#).is_none());
        assert!(foreign_script_expansion("pwsh -NoProfile -File build.ps1").is_none());
        assert!(foreign_script_expansion(r#"powershell.exe -Command "echo plain""#).is_none());
    }

    #[test]
    fn ignores_other_programs_and_mentions() {
        assert!(foreign_script_expansion(r#"node -e "console.log($x)""#).is_none());
        assert!(foreign_script_expansion(r#"echo pwsh -Command "$x""#).is_none());
    }
}
