#[derive(Clone, Debug)]
pub(super) enum TokenKind {
    Ident(String),
    String(String),
    Number(String),
    Symbol(char),
    Op(&'static str),
}

#[derive(Clone, Debug)]
pub(super) struct Token {
    pub(super) kind: TokenKind,
    pub(super) byte: usize,
}

pub(super) fn lex(input: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut pos = 0usize;
    while pos < input.len() {
        let ch = input[pos..].chars().next().unwrap();
        if ch.is_whitespace() {
            pos += ch.len_utf8();
            continue;
        }
        let byte = pos;
        if is_ident_start(ch) {
            pos += ch.len_utf8();
            while pos < input.len() {
                let next = input[pos..].chars().next().unwrap();
                if is_ident_continue(next) {
                    pos += next.len_utf8();
                } else {
                    break;
                }
            }
            tokens.push(Token {
                kind: TokenKind::Ident(input[byte..pos].to_string()),
                byte,
            });
            continue;
        }
        if ch.is_ascii_digit() {
            pos += 1;
            while pos < input.len() {
                let next = input.as_bytes()[pos] as char;
                if next.is_ascii_digit() {
                    pos += 1;
                } else {
                    break;
                }
            }
            tokens.push(Token {
                kind: TokenKind::Number(input[byte..pos].to_string()),
                byte,
            });
            continue;
        }
        if ch == '"' {
            pos += 1;
            let mut out = String::new();
            loop {
                if pos >= input.len() {
                    return Err(format!(
                        "managed dex DSL parse error at byte {}: unterminated string",
                        byte
                    ));
                }
                let current = input[pos..].chars().next().unwrap();
                pos += current.len_utf8();
                match current {
                    '"' => break,
                    '\\' => {
                        if pos >= input.len() {
                            return Err(format!(
                                "managed dex DSL parse error at byte {}: unterminated string escape",
                                pos
                            ));
                        }
                        let escaped = input[pos..].chars().next().unwrap();
                        pos += escaped.len_utf8();
                        match escaped {
                            '"' => out.push('"'),
                            '\\' => out.push('\\'),
                            'n' => out.push('\n'),
                            'r' => out.push('\r'),
                            't' => out.push('\t'),
                            other => {
                                return Err(format!(
                                    "managed dex DSL parse error at byte {}: unsupported string escape \\{}",
                                    pos - other.len_utf8(),
                                    other
                                ));
                            }
                        }
                    }
                    other => out.push(other),
                }
            }
            tokens.push(Token {
                kind: TokenKind::String(out),
                byte,
            });
            continue;
        }
        let rest = &input[pos..];
        let op = if rest.starts_with(">>>=") {
            Some(">>>=")
        } else if rest.starts_with(">>>") {
            Some(">>>")
        } else if rest.starts_with("<<=") {
            Some("<<=")
        } else if rest.starts_with("<<") {
            Some("<<")
        } else if rest.starts_with(">>=") {
            Some(">>=")
        } else if rest.starts_with(">>") {
            Some(">>")
        } else if rest.starts_with("==") {
            Some("==")
        } else if rest.starts_with("!=") {
            Some("!=")
        } else if rest.starts_with("<=") {
            Some("<=")
        } else if rest.starts_with(">=") {
            Some(">=")
        } else if rest.starts_with("&&") {
            Some("&&")
        } else if rest.starts_with("||") {
            Some("||")
        } else if rest.starts_with("?.") {
            Some("?.")
        } else if rest.starts_with("++") {
            Some("++")
        } else if rest.starts_with("--") {
            Some("--")
        } else if rest.starts_with("+=") {
            Some("+=")
        } else if rest.starts_with("-=") {
            Some("-=")
        } else if rest.starts_with("*=") {
            Some("*=")
        } else if rest.starts_with("/=") {
            Some("/=")
        } else if rest.starts_with("%=") {
            Some("%=")
        } else if rest.starts_with("&=") {
            Some("&=")
        } else if rest.starts_with("|=") {
            Some("|=")
        } else if rest.starts_with("^=") {
            Some("^=")
        } else {
            None
        };
        if let Some(op) = op {
            pos += op.len();
            tokens.push(Token {
                kind: TokenKind::Op(op),
                byte,
            });
            continue;
        }
        if "{}()[];:,.?+-=<>!*/%&|^~".contains(ch) {
            pos += ch.len_utf8();
            tokens.push(Token {
                kind: TokenKind::Symbol(ch),
                byte,
            });
            continue;
        }
        return Err(format!(
            "managed dex DSL parse error at byte {}: unexpected character '{}'",
            byte, ch
        ));
    }
    Ok(tokens)
}

fn is_ident_start(ch: char) -> bool {
    ch == '$' || ch == '_' || ch.is_ascii_alphabetic()
}

fn is_ident_continue(ch: char) -> bool {
    ch == '$' || ch == '_' || ch.is_ascii_alphanumeric()
}
