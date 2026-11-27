// Flat token stream shared by nasc's lexer and parser, in the spirit of
// One token format end to end: a single representation, no per-stage trees.

#[derive(Debug, Clone, PartialEq)]
pub enum Tok {
    Ident(String),
    Int(u128),
    Bool(bool),
    Str(String),
    Char(char),
    Fn,
    Let,
    If,
    Else,
    While,
    Loop,
    For,
    In,
    Match,
    Return,
    Break,
    Continue,
    Mut,
    Ref,

    Pub,
    Unsafe,
    Const,
    Static,
    Async,
    Extern,
    Crate,
    SelfValue,
    SelfType,
    Struct,
    Enum,
    Impl,
    Trait,
    Mod,
    Use,
    Where,
    Type,
    Dyn,
    Move,
    Underscore,
    // punctuation / operators
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Colon,
    ColonColon,
    Dot,
    DotDot,
    DotDotEq,
    Arrow,
    FatArrow,
    Eq,
    EqEq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    AmpAmp,
    Pipe,
    PipePipe,
    Caret,
    Shl,
    Shr,
    Not,
    Question,
    PlusEq,
    MinusEq,
    StarEq,
    SlashEq,
    PercentEq,
    AmpEq,
    PipeEq,
    CaretEq,
    ShlEq,
    ShrEq,
    Hash,
    Attr,
    Lifetime(String),
    Eof,
}

pub struct Lexer<'a> {
    src: &'a [u8],
    pos: usize,
}

pub fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let mut lx = Lexer { src: src.as_bytes(), pos: 0 };
    let mut out = Vec::with_capacity(src.len() / 4);
    loop {
        let t = lx.next()?;
        let end = t == Tok::Eof;
        out.push(t);
        if end {
            return Ok(out);
        }
    }
}

impl<'a> Lexer<'a> {
    fn peek(&self) -> Option<u8> {
        self.src.get(self.pos).copied()
    }
    fn peek2(&self) -> Option<u8> {
        self.src.get(self.pos + 1).copied()
    }
    fn bump(&mut self) -> Option<u8> {
        let c = self.peek();
        self.pos += usize::from(c.is_some());
        c
    }
    fn skip_trivia(&mut self) {
        loop {
            match self.peek() {
                Some(b) if b.is_ascii_whitespace() => {
                    self.pos += 1;
                }
                Some(b'/') if self.peek2() == Some(b'/') => {
                    while self.peek().is_some() && self.peek() != Some(b'\n') {
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek2() == Some(b'*') => {
                    self.pos += 2;
                    let mut depth = 1usize;
                    while depth > 0 {
                        match (self.peek(), self.peek2()) {
                            (Some(b'*'), Some(b'/')) => {
                                self.pos += 2;
                                depth -= 1;
                            }
                            (Some(b'/'), Some(b'*')) => {
                                self.pos += 2;
                                depth += 1;
                            }
                            (Some(_), _) => self.pos += 1,
                            (None, _) => return,
                        }
                    }
                }
                _ => return,
            }
        }
    }

    fn ident(&mut self) -> String {
        let start = self.pos;
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' {
                self.pos += 1;
            } else {
                break;
            }
        }
        String::from_utf8_lossy(&self.src[start..self.pos]).into_owned()
    }

    fn string(&mut self) -> Result<String, String> {
        // consume opening quote already handled by caller? we handle it here
        self.pos += 1; // opening "
        let mut out = String::new();
        loop {
            match self.bump() {
                None => return Err("unterminated string".into()),
                Some(b'"') => return Ok(out),
                Some(b'\\') => match self.bump() {
                    Some(b'n') => out.push('\n'),
                    Some(b't') => out.push('\t'),
                    Some(b'r') => out.push('\r'),
                    Some(b'\\') => out.push('\\'),
                    Some(b'"') => out.push('"'),
                    Some(b'0') => out.push('\0'),
                    Some(b'x') => {
                        let h1 = self.bump().ok_or("bad escape")?;
                        let h2 = self.bump().ok_or("bad escape")?;
                        let v = u8::from_str_radix(
                            &format!("{}{}", h1 as char, h2 as char),
                            16,
                        )
                        .map_err(|_| "bad hex escape".to_string())?;
                        out.push(v as char);
                    }
                    Some(b'u') => {
                        if self.bump() != Some(b'{') {
                            return Err("bad unicode escape".into());
                        }
                        let mut h = String::new();
                        while self.peek() != Some(b'}') {
                            h.push(self.bump().ok_or("bad unicode escape")? as char);
                        }
                        self.pos += 1;
                        let cp = u32::from_str_radix(&h, 16)
                            .map_err(|_| "bad unicode escape".to_string())?;
                        out.push(
                            char::from_u32(cp).ok_or("bad unicode scalar")?,
                        );
                    }
                    other => return Err(format!("bad escape \\{}", other.unwrap_or(b'?') as char)),
                },
                Some(c) => out.push(c as char),
            }
        }
    }

    fn char_lit(&mut self) -> Result<char, String> {
        self.pos += 1; // opening '
        let c = match self.bump() {
            None => return Err("unterminated char".into()),
            Some(b'\\') => match self.bump() {
                Some(b'n') => '\n',
                Some(b't') => '\t',
                Some(b'r') => '\r',
                Some(b'\\') => '\\',
                Some(b'\'') => '\'',
                Some(b'0') => '\0',
                other => return Err(format!("bad char escape \\{}", other.unwrap_or(b'?') as char)),
            },
            Some(c) => c as char,
        };
        if self.bump() != Some(b'\'') {
            return Err("unterminated char literal".into());
        }
        Ok(c)
    }

    fn number(&mut self) -> Result<Tok, String> {
        let start = self.pos;
        if self.peek() == Some(b'0')
            && matches!(self.peek2(), Some(b'x') | Some(b'b') | Some(b'o'))
        {
            let radix = match self.src[self.pos + 1] {
                b'x' => 16,
                b'b' => 2,
                b'o' => 8,
                _ => unreachable!(),
            };
            self.pos += 2;
            let ds = self.pos;
            while let Some(b) = self.peek() {
                if b.is_ascii_alphanumeric() || b == b'_' {
                    self.pos += 1;
                } else {
                    break;
                }
            }
            let text: String = String::from_utf8_lossy(&self.src[ds..self.pos])
                .chars()
                .filter(|c| *c != '_')
                .collect();
            // strip a primitive suffix
            let digits: String = text
                .chars()
                .take_while(|c| c.is_digit(radix))
                .collect();
            let v = u128::from_str_radix(&digits, radix)
                .map_err(|_| format!("bad literal at byte {start}"))?;
            return Ok(Tok::Int(v));
        }
        while let Some(b) = self.peek() {
            if b.is_ascii_alphanumeric() || b == b'_' || b == b'.' {
                // avoid consuming `..` after an int
                if b == b'.'
                    && (self.peek2() == Some(b'.') || !self.src.get(self.pos + 1).is_some_and(|c| c.is_ascii_digit()))
                {
                    break;
                }
                self.pos += 1;
            } else {
                break;
            }
        }
        let text: String = String::from_utf8_lossy(&self.src[start..self.pos])
            .chars()
            .filter(|c| *c != '_')
            .collect();
        // strip primitive suffix (i32, u64, usize, ...)
        let digits: String = text
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        if digits.contains('.') {
            return Err(format!("float literal {digits} unsupported"));
        }
        digits
            .parse::<u128>()
            .map(Tok::Int)
            .map_err(|_| format!("bad literal {text}"))
    }

    pub fn next(&mut self) -> Result<Tok, String> {
        self.skip_trivia();
        let Some(b) = self.peek() else {
            return Ok(Tok::Eof);
        };
        // raw string r"..." or r#"..."#
        if b == b'r' {
            let mut p = self.pos + 1;
            let mut hashes = 0usize;
            while self.src.get(p) == Some(&b'#') {
                hashes += 1;
                p += 1;
            }
            if self.src.get(p) == Some(&b'"') {
                self.pos = p + 1;
                let start = self.pos;
                loop {
                    match self.peek() {
                        None => return Err("unterminated raw string".into()),
                        Some(b'"') => {
                            let mut ok = true;
                            for i in 0..hashes {
                                if self.src.get(self.pos + 1 + i) != Some(&b'#') {
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                let s = String::from_utf8_lossy(
                                    &self.src[start..self.pos],
                                )
                                .into_owned();
                                self.pos += 1 + hashes;
                                return Ok(Tok::Str(s));
                            }
                            self.pos += 1;
                        }
                        _ => self.pos += 1,
                    }
                }
            }
        }
        if b == b'"' {
            return self.string().map(Tok::Str);
        }
        if b == b'\'' {
            // lifetime vs char literal: 'a lifetime follows ident and no closing quote
            if matches!(self.peek2(), Some(c) if c.is_ascii_alphabetic() || c == b'_') {
                // char literal has exactly one char then '
                let save = self.pos;
                match self.char_lit() {
                    Ok(c) => return Ok(Tok::Char(c)),
                    Err(_) => {
                        self.pos = save;
                    }
                }
            }
            if matches!(self.peek2(), Some(c) if c.is_ascii_alphabetic() || c == b'_') {
                self.pos += 1;
                return Ok(Tok::Lifetime(self.ident()));
            }
            return Err("stray '".into());
        }
        if b.is_ascii_digit() {
            return self.number();
        }
        if b.is_ascii_alphabetic() || b == b'_' {
            let id = self.ident();
            return Ok(match id.as_str() {
                "fn" => Tok::Fn,
                "let" => Tok::Let,
                "if" => Tok::If,
                "else" => Tok::Else,
                "while" => Tok::While,
                "loop" => Tok::Loop,
                "for" => Tok::For,
                "in" => Tok::In,
                "match" => Tok::Match,
                "return" => Tok::Return,
                "break" => Tok::Break,
                "continue" => Tok::Continue,
                "mut" => Tok::Mut,
                "ref" => Tok::Ref,
                "true" => Tok::Bool(true),
                "false" => Tok::Bool(false),
                "pub" => Tok::Pub,
                "unsafe" => Tok::Unsafe,
                "const" => Tok::Const,
                "static" => Tok::Static,
                "async" => Tok::Async,
                "extern" => Tok::Extern,
                "crate" => Tok::Crate,
                "self" => Tok::SelfValue,
                "Self" => Tok::SelfType,
                "struct" => Tok::Struct,
                "enum" => Tok::Enum,
                "impl" => Tok::Impl,
                "trait" => Tok::Trait,
                "mod" => Tok::Mod,
                "use" => Tok::Use,
                "where" => Tok::Where,
                "type" => Tok::Type,
                "dyn" => Tok::Dyn,
                "move" => Tok::Move,
                "_" => Tok::Underscore,
                _ => Tok::Ident(id),
            });
        }
        self.pos += 1;
        let t = match b {
            b'(' => Tok::LParen,
            b')' => Tok::RParen,
            b'{' => Tok::LBrace,
            b'}' => Tok::RBrace,
            b'[' => Tok::LBracket,
            b']' => Tok::RBracket,
            b',' => Tok::Comma,
            b';' => Tok::Semi,
            b'#' => {
                if self.peek() == Some(b'[') {
                    self.pos += 1;
                    Tok::Attr
                } else {
                    Tok::Hash
                }
            }
            b'?' => Tok::Question,
            b'!' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::Ne
                } else {
                    Tok::Not
                }
            }
            b'.' => {
                if self.peek() == Some(b'.') {
                    self.pos += 1;
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        Tok::DotDotEq
                    } else {
                        Tok::DotDot
                    }
                } else {
                    Tok::Dot
                }
            }
            b':' => {
                if self.peek() == Some(b':') {
                    self.pos += 1;
                    Tok::ColonColon
                } else {
                    Tok::Colon
                }
            }
            b'=' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::EqEq
                } else if self.peek() == Some(b'>') {
                    self.pos += 1;
                    Tok::FatArrow
                } else {
                    Tok::Eq
                }
            }
            b'-' => {
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                    Tok::Arrow
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::MinusEq
                } else {
                    Tok::Minus
                }
            }
            b'+' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::PlusEq
                } else {
                    Tok::Plus
                }
            }
            b'*' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::StarEq
                } else {
                    Tok::Star
                }
            }
            b'/' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::SlashEq
                } else {
                    Tok::Slash
                }
            }
            b'%' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::PercentEq
                } else {
                    Tok::Percent
                }
            }
            b'<' => {
                if self.peek() == Some(b'<') {
                    self.pos += 1;
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        Tok::ShlEq
                    } else {
                        Tok::Shl
                    }
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::Le
                } else {
                    Tok::Lt
                }
            }
            b'>' => {
                if self.peek() == Some(b'>') {
                    self.pos += 1;
                    if self.peek() == Some(b'=') {
                        self.pos += 1;
                        Tok::ShrEq
                    } else {
                        Tok::Shr
                    }
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::Ge
                } else {
                    Tok::Gt
                }
            }
            b'&' => {
                if self.peek() == Some(b'&') {
                    self.pos += 1;
                    Tok::AmpAmp
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::AmpEq
                } else {
                    Tok::Amp
                }
            }
            b'|' => {
                if self.peek() == Some(b'|') {
                    self.pos += 1;
                    Tok::PipePipe
                } else if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::PipeEq
                } else {
                    Tok::Pipe
                }
            }
            b'^' => {
                if self.peek() == Some(b'=') {
                    self.pos += 1;
                    Tok::CaretEq
                } else {
                    Tok::Caret
                }
            }
            _ => return Err(format!("unexpected byte 0x{b:02x}")),
        };
        Ok(t)
    }
}
