//! Recursive-descent parser for nasc's Rust subset. Items are collected per
//! file; function bodies parse to an expression tree or refuse with a
//! typed `Unsupported` error so callers can fall back to the rustc path.

use crate::lex::{lex, Tok};

#[derive(Debug)]
pub enum Unsupported {
    Msg(String),
}

impl std::fmt::Display for Unsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unsupported::Msg(m) => write!(f, "{m}"),
        }
    }
}
impl std::error::Error for Unsupported {}
type Res<T> = Result<T, Unsupported>;
fn bad<T>(m: impl Into<String>) -> Res<T> {
    Err(Unsupported::Msg(m.into()))
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    Bool,
    Int(&'static str), // i8..i128, u8..u128, isize, usize
    Str,               // &str  (ptr, len)
    Unit,
    Ref(Box<Ty>),
    Tuple(Vec<Ty>),
    Named(String),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(u128),
    Bool(bool),
    Str(String),
    Char(char),
    Unit,
    Var(String),
    Path(Vec<String>),
    Tuple(Vec<Expr>),
    Array(Vec<Expr>),
    Unary(UnOp, Box<Expr>),
    Bin(BinOp, Box<Expr>, Box<Expr>),
    Ref(Box<Expr>, bool),
    Deref(Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Method(Box<Expr>, String, Vec<Expr>),
    Index(Box<Expr>, Box<Expr>),
    Field(Box<Expr>, String),
    StructLit(Vec<String>, Vec<(String, Expr)>),
    If(Box<Expr>, Box<Expr>, Option<Box<Expr>>),
    Match(Box<Expr>, Vec<Arm>),
    Block(Vec<Stmt>, Option<Box<Expr>>),
    Loop(Box<Expr>),
    While(Box<Expr>, Box<Expr>),
    For(Pat, Box<Expr>, Box<Expr>),
    Break(Option<Box<Expr>>),
    Continue,
    Return(Option<Box<Expr>>),
    Assign(Box<Expr>, Box<Expr>),
    AssignOp(BinOp, Box<Expr>, Box<Expr>),
    Range(Box<Expr>, Box<Expr>, bool),
    As(Box<Expr>, Ty),
    Closure(Vec<String>, Box<Expr>),
    /// Parsed placeholder for constructs outside the subset (macros,
    /// exotic patterns). emit() refuses these, not the parser.
    Unsupported(String),
}

#[derive(Debug, Clone)]
pub struct Arm {
    pub pats: Vec<Pat>,
    pub body: Expr,
}

#[derive(Debug, Clone)]
pub enum Pat {
    Wild,
    Int(u128),
    Bool(bool),
    Char(char),
    Str(String),
    Ident(String),
    Ref(Box<Pat>, bool),
    Tuple(Vec<Pat>),
    Or(Vec<Pat>),
    Path(Vec<String>),
    Range(u128, u128),
    Rest,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let(Pat, Option<Ty>, Option<Expr>),
    Expr(Expr),
    Semi(Expr),
    Item,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    And,
    Or,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy)]
pub enum UnOp {
    Neg,
    Not,
}

#[derive(Debug)]
pub struct FnItem {
    pub name: String,
    pub generic: bool,
    pub params: Vec<(String, Ty)>,
    pub ret: Ty,
    pub body: Expr,
    pub is_unsafe: bool,
    pub is_const: bool,
    pub is_async: bool,
}

pub struct P {
    toks: Vec<Tok>,
    i: usize,
    no_struct_lit: bool,
}

pub fn parse_file(src: &str) -> Res<Vec<FnItem>> {
    let toks = lex(src).map_err(Unsupported::Msg)?;
    let mut p = P { toks, i: 0, no_struct_lit: false };
    p.items()
}

impl P {
    fn peek(&self) -> &Tok {
        &self.toks[self.i]
    }
    fn peek2(&self) -> &Tok {
        self.toks.get(self.i + 1).unwrap_or(&Tok::Eof)
    }
    fn bump(&mut self) -> Tok {
        let t = self.toks[self.i].clone();
        self.i += 1;
        t
    }
    fn eat(&mut self, t: &Tok) -> bool {
        if self.peek() == t {
            self.i += 1;
            true
        } else {
            false
        }
    }
    fn expect(&mut self, t: Tok) -> Res<()> {
        if self.eat(&t) {
            Ok(())
        } else {
            bad(format!("expected {t:?}, got {:?}", self.peek()))
        }
    }
    fn ident(&mut self) -> Res<String> {
        match self.bump() {
            Tok::Ident(s) => Ok(s),
            other => bad(format!("expected ident, got {other:?}")),
        }
    }

    /// Skip a balanced token tree starting at the current open delimiter.
    fn skip_tree(&mut self, open: Tok, close: Tok) -> Res<()> {
        self.expect(open.clone())?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.bump() {
                Tok::Eof => return bad("eof inside token tree"),
                t if t == open => depth += 1,
                t if t == close => depth -= 1,
                Tok::LParen | Tok::LBrace | Tok::LBracket => {
                    // nested different delimiters need their own depth
                    let (o, c) = match self.toks[self.i - 1] {
                        Tok::LParen => (Tok::LParen, Tok::RParen),
                        Tok::LBrace => (Tok::LBrace, Tok::RBrace),
                        _ => (Tok::LBracket, Tok::RBracket),
                    };
                    let mut d = 1usize;
                    while d > 0 {
                        match self.bump() {
                            Tok::Eof => return bad("eof inside token tree"),
                            ref t if *t == o => d += 1,
                            ref t if *t == c => d -= 1,
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn items(&mut self) -> Res<Vec<FnItem>> {
        self.items_until(None, "")
    }

    /// Parse items until `}` (nested) or EOF. `qual` prefixes fn names with
    /// the enclosing `mod`/`impl` path so selectors match source paths.
    fn items_until(&mut self, close: Option<&Tok>, qual: &str) -> Res<Vec<FnItem>> {
        let mut fns = Vec::new();
        let qualify = |name: String| {
            if qual.is_empty() { name } else { format!("{qual}::{name}") }
        };
        loop {
            if let Some(c) = close {
                if self.peek() == c {
                    self.bump();
                    return Ok(fns);
                }
            }
            match self.peek() {
                Tok::Eof => return Ok(fns),
                Tok::Attr => {
                    self.bump();
                    // attribute contents: balanced token tree
                    let mut depth = 1usize;
                    while depth > 0 {
                        match self.bump() {
                            Tok::Eof => return bad("eof in attribute"),
                            Tok::LBracket => depth += 1,
                            Tok::RBracket => depth -= 1,
                            _ => {}
                        }
                    }
                }
                Tok::Pub => {
                    self.bump();
                    // pub(restricted)?
                    if self.peek() == &Tok::LParen {
                        self.skip_tree(Tok::LParen, Tok::RParen)?;
                    }
                }
                Tok::Fn | Tok::Unsafe | Tok::Const | Tok::Async | Tok::Extern => {
                    if let Some(f) = self.maybe_fn()? {
                        let mut f = f;
                        f.name = qualify(f.name);
                        fns.push(f);
                    }
                }
                Tok::Mod => {
                    self.bump();
                    let name = self.ident().unwrap_or_default();
                    match self.peek() {
                        Tok::LBrace => {
                            self.bump();
                            let q = qualify(name);
                            let mut inner = self.items_until(Some(&Tok::RBrace), &q)?;
                            fns.append(&mut inner);
                        }
                        Tok::Semi => {
                            self.bump();
                        }
                        _ => self.skip_item()?,
                    }
                }
                Tok::Impl => {
                    // impl <T> for U { ... } or impl U { ... }: prefix inner
                    // fns with the type's leaf name. Generic params skipped.
                    self.bump();
                    if self.peek() == &Tok::Lt {
                        self.skip_generic()?;
                    }
                    let mut tyname = String::new();
                    loop {
                        match self.peek().clone() {
                            Tok::For => {
                                self.bump();
                                // `for <Type>`: use the *target* type
                                tyname = String::new();
                            }
                            Tok::Ident(n) => {
                                tyname = n;
                                self.bump();
                            }
                            Tok::ColonColon | Tok::Lt | Tok::Gt => {
                                self.bump();
                            }
                            Tok::LBrace => {
                                self.bump();
                                let q = qualify(tyname);
                                let mut inner = self.items_until(Some(&Tok::RBrace), &q)?;
                                fns.append(&mut inner);
                                break;
                            }
                            Tok::Semi => {
                                self.bump();
                                break;
                            }
                            _ => {
                                self.bump();
                            }
                        }
                    }
                }
                Tok::Struct | Tok::Enum | Tok::Use | Tok::Trait
                | Tok::Type | Tok::Static => {
                    // skip the whole item: to matching top-level close or ;
                    self.skip_item()?;
                }
                Tok::Ident(_) => {
                    // macro call like foo!{...} or item-ish; skip conservatively
                    self.skip_item()?;
                }
                _ => self.skip_item()?,
            }
        }
    }

    /// After a failed `block_expr` the cursor is inside the function
    /// body; consume tokens until depth reaches the closing `}`.
    fn resync_block(&mut self) {
        // We can't reliably recover position inside the brace, so this
        // parser becomes skip-only: walk to EOF is unsafe, instead walk
        // balanced braces until a `}` at depth 0 beyond our entry.
        // Track depth by counting `{`/`}`; entry depth is 1 (the body's
        // opening brace was already consumed or about to be).
        let mut depth = 0usize;
        let mut started = false;
        loop {
            match self.peek() {
                Tok::Eof => return,
                Tok::LBrace => {
                    depth += 1;
                    started = true;
                    self.bump();
                }
                Tok::RBrace => {
                    self.bump();
                    if !started {
                        return; // this `}` closed our body
                    }
                    depth -= 1;
                    if depth == 0 {
                        return;
                    }
                }
                _ => { self.bump(); }
            }
        }
    }

    fn skip_item(&mut self) -> Res<()> {
        // consume tokens until `;` at depth 0 or a balanced `{}` item end.
        self.bump();
        loop {
            match self.peek() {
                Tok::Eof => return Ok(()),
                Tok::Semi => {
                    self.bump();
                    return Ok(());
                }
                Tok::LBrace => {
                    self.skip_tree(Tok::LBrace, Tok::RBrace)?;
                    return Ok(());
                }
                Tok::LParen => self.skip_tree(Tok::LParen, Tok::RParen)?,
                Tok::LBracket => self.skip_tree(Tok::LBracket, Tok::RBracket)?,
                _ => {
                    self.bump();
                }
            }
        }
    }

    /// At Fn/Unsafe/Const/Async/Extern. Returns None if it was e.g. `static`.
    fn maybe_fn(&mut self) -> Res<Option<FnItem>> {
        let mut is_unsafe = false;
        let mut is_const = false;
        let mut is_async = false;
        loop {
            match self.peek() {
                Tok::Unsafe => {
                    is_unsafe = true;
                    self.bump();
                }
                Tok::Const => {
                    is_const = true;
                    self.bump();
                }
                Tok::Async => {
                    is_async = true;
                    self.bump();
                }
                Tok::Extern => {
                    self.bump();
                    // extern "C"
                    if matches!(self.peek(), Tok::Str(_)) {
                        self.bump();
                    }
                }
                Tok::Fn => {
                    self.bump();
                    break;
                }
                _ => return Ok(None), // const/static handled by caller fallback
            }
        }
        let name = self.ident()?;
        let mut generic = false;
        if self.peek() == &Tok::Lt {
            generic = true;
            self.skip_generic()?;
        }
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        if self.peek() != &Tok::RParen {
            loop {
                // patterns: ident, mut ident, _: ty, &self, self
                match self.peek().clone() {
                    Tok::Underscore => {
                        self.bump();
                        self.expect(Tok::Colon)?;
                        let t = self.ty()?;
                        params.push((format!("_p{}", params.len()), t));
                    }
                    Tok::Mut => {
                        self.bump();
                        let id = self.ident()?;
                        self.expect(Tok::Colon)?;
                        let t = self.ty()?;
                        params.push((id, t));
                    }
                    Tok::Ident(_) => {
                        let id = self.ident()?;
                        if self.peek() == &Tok::Colon {
                            self.bump();
                            let t = self.ty()?;
                            params.push((id, t));
                        } else {
                            return bad("param without type");
                        }
                    }
                    Tok::Amp | Tok::SelfValue => {
                        // &self / &mut self / self
                        return bad("self param unsupported");
                    }
                    _ => return bad("param pattern unsupported"),
                }
                if !self.eat(&Tok::Comma) {
                    break;
                }
                if self.peek() == &Tok::RParen {
                    break;
                }
            }
        }
        self.expect(Tok::RParen)?;
        let ret = if self.eat(&Tok::Arrow) {
            self.ty()?
        } else {
            Ty::Unit
        };
        if self.peek() == &Tok::Where {
            return bad("where clause unsupported");
        }
        let body = if self.peek() == &Tok::Semi {
            self.bump();
            return Ok(None); // declaration only
        } else {
            // One bad body must not sink the whole file: mark it
            // unsupported and skip the remaining tokens to `}` so other
            // functions in the same source still compile.
            match self.block_expr() {
                Ok(b) => b,
                Err(_) => {
                    self.resync_block();
                    Expr::Unsupported("body".into())
                }
            }
        };
        Ok(Some(FnItem {
            name,
            generic,
            params,
            ret,
            body,
            is_unsafe,
            is_const,
            is_async,
        }))
    }

    fn skip_generic(&mut self) -> Res<()> {
        // < ... > with nested <> for qualified paths
        self.expect(Tok::Lt)?;
        let mut depth = 1usize;
        while depth > 0 {
            match self.bump() {
                Tok::Eof => return bad("eof in generics"),
                Tok::Lt => depth += 1,
                Tok::Gt => depth -= 1,
                Tok::Shr => depth = depth.saturating_sub(2),
                _ => {}
            }
        }
        Ok(())
    }

    fn ty(&mut self) -> Res<Ty> {
        match self.bump() {
            Tok::LParen => {
                if self.eat(&Tok::RParen) {
                    return Ok(Ty::Unit);
                }
                let mut ts = vec![self.ty()?];
                while self.eat(&Tok::Comma) {
                    if self.peek() == &Tok::RParen {
                        break;
                    }
                    ts.push(self.ty()?);
                }
                self.expect(Tok::RParen)?;
                Ok(if ts.len() == 1 { ts.pop().unwrap() } else { Ty::Tuple(ts) })
            }
            Tok::Amp => {
                if matches!(self.peek(), Tok::Lifetime(_)) {
                    self.bump();
                }
                let mutable = self.eat(&Tok::Mut);
                let inner = self.ty()?;
                if inner == Ty::Str && !mutable {
                    return Ok(Ty::Str);
                }
                Ok(Ty::Ref(Box::new(inner)))
            }
            Tok::Ident(n) if n == "str" => Ok(Ty::Str),
            Tok::Ident(n) => {
                // path with :: and generics
                let mut path = n;
                while self.peek() == &Tok::ColonColon {
                    self.bump();
                    let seg = self.ident().map_err(|_| Unsupported::Msg("bad path".into()))?;
                    path.push_str("::");
                    path.push_str(&seg);
                }
                if self.peek() == &Tok::Lt {
                    self.skip_generic()?;
                    path.push_str("<..>");
                }
                Ok(match path.as_str() {
                    "bool" => Ty::Bool,
                    "i8" | "i16" | "i32" | "i64" | "i128" | "isize" | "u8" | "u16"
                    | "u32" | "u64" | "u128" | "usize" => {
                        Ty::Int(match path.as_str() {
                            "i8" => "i8",
                            "i16" => "i16",
                            "i32" => "i32",
                            "i64" => "i64",
                            "i128" => "i128",
                            "isize" => "i64",
                            "u8" => "i8",
                            "u16" => "i16",
                            "u32" => "i32",
                            "u64" | "usize" => "i64",
                            "u128" => "i128",
                            _ => unreachable!(),
                        })
                    }
                    "str" => Ty::Named(path),
                    _ => Ty::Named(path),
                })
            }
            Tok::LBracket => {
                let inner = self.ty()?;
                if self.eat(&Tok::Semi) {
                    // sized array [T; N]
                    if let Tok::Int(_) = self.bump() {
                    } else {
                        return bad("array len must be int");
                    }
                }
                self.expect(Tok::RBracket)?;
                Ok(Ty::Named(format!("[{inner:?}]")))
            }
            Tok::Star => {
                // *const T / *mut T
                match self.bump() {
                    Tok::Const | Tok::Mut => {
                        let _ = self.ty()?;
                        Ok(Ty::Named("ptr".into()))
                    }
                    _ => bad("bad pointer type"),
                }
            }
            other => bad(format!("unsupported type start {other:?}")),
        }
    }

    fn block_expr(&mut self) -> Res<Expr> {
        self.expect(Tok::LBrace)?;
        let mut stmts = Vec::new();
        let mut tail = None;
        loop {
            match self.peek().clone() {
                Tok::RBrace => {
                    self.bump();
                    return Ok(Expr::Block(stmts, tail));
                }
                Tok::Eof => return bad("eof in block"),
                Tok::Let => {
                    self.bump();
                    let _mutable = self.eat(&Tok::Mut);
                    let pat = self.pattern()?;
                    let ty = if self.eat(&Tok::Colon) {
                        Some(self.ty()?)
                    } else {
                        None
                    };
                    let init = if self.eat(&Tok::Eq) {
                        Some(self.expr()?)
                    } else {
                        None
                    };
                    // optional else for let-else
                    if self.peek() == &Tok::Else {
                        return bad("let-else unsupported");
                    }
                    self.expect(Tok::Semi)?;
                    stmts.push(Stmt::Let(pat, ty, init));
                }
                Tok::Attr => {
                    self.bump();
                    let mut d = 1usize;
                    while d > 0 {
                        match self.bump() {
                            Tok::Eof => return bad("eof in attr"),
                            Tok::LBracket => d += 1,
                            Tok::RBracket => d -= 1,
                            _ => {}
                        }
                    }
                }
                Tok::Semi => {
                    self.bump();
                }
                _ => {
                    let e = match self.expr() {
                        Ok(e) => e,
                        Err(_) => Expr::Unsupported("stmt".into()),
                    };
                    match self.peek() {
                        Tok::Semi => {
                            self.bump();
                            stmts.push(Stmt::Expr(e));
                        }
                        Tok::RBrace => {
                            tail = Some(Box::new(e));
                        }
                        _ => {
                            // expression statement without trailing semi only
                            // for control-flow forms; require semi otherwise
                            match e {
                                Expr::If(..) | Expr::Match(..) | Expr::Loop(_)
                                | Expr::While(..) | Expr::For(..) | Expr::Block(..) => {
                                    stmts.push(Stmt::Expr(e));
                                }
                                _ => return bad(format!("expected ; or }} after expr, got {:?}", self.peek())),
                            }
                        }
                    }
                }
            }
        }
    }

    fn pattern(&mut self) -> Res<Pat> {
        let mut pats = vec![self.pat_single()?];
        while self.eat(&Tok::Pipe) {
            pats.push(self.pat_single()?);
        }
        Ok(if pats.len() == 1 {
            pats.pop().unwrap()
        } else {
            Pat::Or(pats)
        })
    }

    fn pat_single(&mut self) -> Res<Pat> {
        match self.bump() {
            Tok::Underscore => Ok(Pat::Wild),
            Tok::Int(v) => {
                if self.peek() == &Tok::DotDotEq {
                    self.bump();
                    if let Tok::Int(hi) = self.bump() {
                        return Ok(Pat::Range(v, hi));
                    }
                    return bad("bad range pattern");
                }
                Ok(Pat::Int(v))
            }
            Tok::Bool(b) => Ok(Pat::Bool(b)),
            Tok::Char(c) => Ok(Pat::Char(c)),
            Tok::Str(s) => Ok(Pat::Str(s)),
            Tok::Ref => Ok(Pat::Ref(Box::new(self.pat_single()?), false)),
            Tok::Amp => {
                if self.eat(&Tok::Mut) {
                    Ok(Pat::Ref(Box::new(self.pat_single()?), true))
                } else {
                    Ok(Pat::Ref(Box::new(self.pat_single()?), false))
                }
            }
            Tok::Mut => Ok(Pat::Ident(format!("mut {}", self.ident()?))),
            Tok::Ident(n) => {
                let mut path = vec![n];
                while self.peek() == &Tok::ColonColon {
                    self.bump();
                    path.push(self.ident().map_err(|_| Unsupported::Msg("path".into()))?);
                }
                if path.len() > 1 {
                    // possible tuple-struct pattern Name(..) or path
                    if self.peek() == &Tok::LParen {
                        return bad("tuple-struct pattern unsupported");
                    }
                    Ok(Pat::Path(path))
                } else {
                    let n = path.pop().unwrap();
                    if self.peek() == &Tok::LParen {
                        // tuple-struct pattern
                        self.bump();
                        let mut ps = Vec::new();
                        if self.peek() != &Tok::RParen {
                            loop {
                                if self.peek() == &Tok::DotDot {
                                    self.bump();
                                    ps.push(Pat::Rest);
                                } else {
                                    ps.push(self.pattern()?);
                                }
                                if !self.eat(&Tok::Comma) {
                                    break;
                                }
                            }
                        }
                        self.expect(Tok::RParen)?;
                        return bad("tuple-struct pattern unsupported");
                    }
                    Ok(if n == "mut" { Pat::Wild } else { Pat::Ident(n) })
                }
            }
            Tok::LParen => {
                let mut ps = Vec::new();
                if self.peek() != &Tok::RParen {
                    loop {
                        ps.push(self.pattern()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RParen)?;
                Ok(Pat::Tuple(ps))
            }
            other => bad(format!("unsupported pattern {other:?}")),
        }
    }

    /// Pratt-style expression parser.
    pub fn expr(&mut self) -> Res<Expr> {
        self.expr_bp(0)
    }

    /// Rust resolves `if X {`/for-iterand ambiguity by banning struct
    /// literals in that operand; expr_no_lit applies that rule.
    fn expr_no_lit(&mut self) -> Res<Expr> {
        let old = self.no_struct_lit;
        self.no_struct_lit = true;
        let e = self.expr_bp(0);
        self.no_struct_lit = old;
        e
    }

    fn expr_bp(&mut self, min_bp: u8) -> Res<Expr> {
        let mut lhs = self.unary()?;
        loop {
            let (l_bp, op) = match self.peek() {
                Tok::PipePipe => (1, Some(BinOp::Or)),
                Tok::AmpAmp => (3, Some(BinOp::And)),
                Tok::EqEq => (5, Some(BinOp::Eq)),
                Tok::Ne => (5, Some(BinOp::Ne)),
                Tok::Lt => (7, Some(BinOp::Lt)),
                Tok::Le => (7, Some(BinOp::Le)),
                Tok::Gt => (7, Some(BinOp::Gt)),
                Tok::Ge => (7, Some(BinOp::Ge)),
                Tok::Pipe => (9, Some(BinOp::BitOr)),
                Tok::Caret => (11, Some(BinOp::BitXor)),
                Tok::Amp => (13, Some(BinOp::BitAnd)),
                Tok::Shl => (15, Some(BinOp::Shl)),
                Tok::Shr => (15, Some(BinOp::Shr)),
                Tok::Plus => (17, Some(BinOp::Add)),
                Tok::Minus => (17, Some(BinOp::Sub)),
                Tok::Star => (19, Some(BinOp::Mul)),
                Tok::Slash => (19, Some(BinOp::Div)),
                Tok::Percent => (19, Some(BinOp::Rem)),
                Tok::Ident(k) if k == "as" => (23, None),
                Tok::Eq => (2, None),
                Tok::PlusEq => (2, None),
                Tok::MinusEq => (2, None),
                Tok::StarEq => (2, None),
                Tok::SlashEq => (2, None),
                Tok::PercentEq => (2, None),
                Tok::AmpEq => (2, None),
                Tok::PipeEq => (2, None),
                Tok::CaretEq => (2, None),
                Tok::ShlEq => (2, None),
                Tok::ShrEq => (2, None),
                Tok::DotDot => (4, None),
                Tok::DotDotEq => (4, None),
                _ => (0, None),
            };
            if l_bp < min_bp {
                break;
            }
            match self.peek() {
                Tok::Eq | Tok::PlusEq | Tok::MinusEq | Tok::StarEq | Tok::SlashEq
                | Tok::PercentEq | Tok::PipeEq | Tok::CaretEq | Tok::ShlEq
                | Tok::ShrEq => {
                    let t = self.bump();
                    let rhs = self.expr_bp(l_bp + 1)?;
                    lhs = match t {
                        Tok::Eq => Expr::Assign(Box::new(lhs), Box::new(rhs)),
                        Tok::PlusEq => Expr::AssignOp(BinOp::Add, Box::new(lhs), Box::new(rhs)),
                        Tok::MinusEq => Expr::AssignOp(BinOp::Sub, Box::new(lhs), Box::new(rhs)),
                        Tok::StarEq => Expr::AssignOp(BinOp::Mul, Box::new(lhs), Box::new(rhs)),
                        Tok::SlashEq => Expr::AssignOp(BinOp::Div, Box::new(lhs), Box::new(rhs)),
                        Tok::PercentEq => Expr::AssignOp(BinOp::Rem, Box::new(lhs), Box::new(rhs)),
                        Tok::PipeEq => Expr::AssignOp(BinOp::BitOr, Box::new(lhs), Box::new(rhs)),
                        Tok::CaretEq => Expr::AssignOp(BinOp::BitXor, Box::new(lhs), Box::new(rhs)),
                        Tok::ShlEq => Expr::AssignOp(BinOp::Shl, Box::new(lhs), Box::new(rhs)),
                        Tok::ShrEq => Expr::AssignOp(BinOp::Shr, Box::new(lhs), Box::new(rhs)),
                        _ => unreachable!(),
                    };
                }
                Tok::Ident(k) if k == "as" => {
                    self.bump();
                    let t = self.ty()?;
                    lhs = Expr::As(Box::new(lhs), t);
                }
                Tok::DotDot | Tok::DotDotEq => {
                    let incl = self.bump() == Tok::DotDotEq;
                    let hi = self.expr_bp(l_bp + 1)?;
                    lhs = Expr::Range(Box::new(lhs), Box::new(hi), incl);
                }
                _ => {
                    let Some(op) = op else { break };
                    self.bump();
                    let rhs = self.expr_bp(l_bp + 1)?;
                    lhs = Expr::Bin(op, Box::new(lhs), Box::new(rhs));
                }
            }
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> Res<Expr> {
        match self.peek().clone() {
            Tok::Minus => {
                self.bump();
                Ok(Expr::Unary(UnOp::Neg, Box::new(self.unary()?)))
            }
            Tok::Not => {
                self.bump();
                Ok(Expr::Unary(UnOp::Not, Box::new(self.unary()?)))
            }
            Tok::Star => {
                self.bump();
                Ok(Expr::Deref(Box::new(self.unary()?)))
            }
            Tok::Amp => {
                self.bump();
                let m = self.eat(&Tok::Mut);
                Ok(Expr::Ref(Box::new(self.unary()?), m))
            }
            Tok::Move => {
                self.bump();
                if self.peek() == &Tok::Pipe || self.peek() == &Tok::PipePipe {
                    self.closure()
                } else {
                    bad("move outside closure")
                }
            }
            Tok::Pipe => self.closure(),
            Tok::PipePipe => self.closure(),
            _ => self.postfix(),
        }
    }

    fn closure(&mut self) -> Res<Expr> {
        let mut params = Vec::new();
        if self.eat(&Tok::PipePipe) {
            // empty
        } else {
            self.expect(Tok::Pipe)?;
            // Params are surface syntax only; any pattern we can't
            // represent is consumed so the body still parses.
            let mut depth = 0usize;
            loop {
                match self.bump() {
                    Tok::Ident(n) => params.push(n),
                    Tok::Mut => params.push(format!("mut {}", self.ident()?)),
                    Tok::LParen | Tok::LBracket => depth += 1,
                    Tok::RParen | Tok::RBracket => depth = depth.saturating_sub(1),
                    Tok::Pipe if depth == 0 => break,
                    Tok::Eof => return bad("eof in closure params"),
                    _ => {}
                }
            }
        }
        if self.eat(&Tok::Arrow) {
            let _ = self.ty()?;
        }
        let body = if self.peek() == &Tok::LBrace {
            self.block_expr()?
        } else {
            self.expr()?
        };
        Ok(Expr::Closure(params, Box::new(body)))
    }

    fn postfix(&mut self) -> Res<Expr> {
        let mut e = self.atom()?;
        loop {
            match self.peek() {
                Tok::LParen => {
                    self.bump();
                    let mut args = Vec::new();
                    if self.peek() != &Tok::RParen {
                        loop {
                            args.push(self.expr()?);
                            if !self.eat(&Tok::Comma) {
                                break;
                            }
                        }
                    }
                    self.expect(Tok::RParen)?;
                    e = Expr::Call(Box::new(e), args);
                }
                Tok::LBracket => {
                    self.bump();
                    let ix = self.expr()?;
                    self.expect(Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(ix));
                }
                Tok::Dot => {
                    self.bump();
                    match self.bump() {
                        Tok::Ident(n) => {
                            if self.peek() == &Tok::LParen
                                || self.peek() == &Tok::ColonColon
                            {
                                if self.peek() == &Tok::ColonColon {
                                    self.skip_generic()?;
                                }
                                self.expect(Tok::LParen)?;
                                let mut args = Vec::new();
                                if self.peek() != &Tok::RParen {
                                    loop {
                                        args.push(self.expr()?);
                                        if !self.eat(&Tok::Comma) {
                                            break;
                                        }
                                    }
                                }
                                self.expect(Tok::RParen)?;
                                e = Expr::Method(Box::new(e), n, args);
                            } else {
                                e = Expr::Field(Box::new(e), n);
                            }
                        }
                        Tok::Int(n) => {
                            e = Expr::Field(Box::new(e), n.to_string());
                        }
                        other => return bad(format!("bad postfix {other:?}")),
                    }
                }
                Tok::Question => {
                    return bad("? operator unsupported");
                }
                _ => return Ok(e),
            }
        }
    }

    fn atom(&mut self) -> Res<Expr> {
        match self.bump() {
            Tok::Int(v) => Ok(Expr::Int(v)),
            Tok::Bool(b) => Ok(Expr::Bool(b)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Char(c) => Ok(Expr::Char(c)),
            Tok::LParen => {
                if self.eat(&Tok::RParen) {
                    return Ok(Expr::Unit);
                }
                let first = self.expr()?;
                if self.eat(&Tok::Comma) {
                    let mut xs = vec![first];
                    loop {
                        if self.peek() == &Tok::RParen {
                            break;
                        }
                        xs.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    return Ok(Expr::Tuple(xs));
                }
                self.expect(Tok::RParen)?;
                Ok(first)
            }
            Tok::LBracket => {
                let mut xs = Vec::new();
                if self.peek() != &Tok::RBracket {
                    loop {
                        xs.push(self.expr()?);
                        if !self.eat(&Tok::Comma) {
                            break;
                        }
                    }
                }
                self.expect(Tok::RBracket)?;
                Ok(Expr::Array(xs))
            }
            Tok::LBrace => {
                self.i -= 1;
                self.block_expr()
            }
            Tok::If => {
                let c = self.expr_no_lit()?;
                let t = self.block_expr()?;
                let e = if self.eat(&Tok::Else) {
                    Some(Box::new(if self.peek() == &Tok::If {
                        self.bump();
                        let c2 = self.expr()?;
                        let t2 = self.block_expr()?;
                        let e2 = if self.eat(&Tok::Else) {
                            Some(Box::new(self.block_expr()?))
                        } else {
                            None
                        };
                        Expr::If(Box::new(c2), Box::new(t2), e2)
                    } else {
                        self.block_expr()?
                    }))
                } else {
                    None
                };
                Ok(Expr::If(Box::new(c), Box::new(t), e))
            }
            Tok::Match => {
                let scrut = self.expr_no_lit()?;
                self.expect(Tok::LBrace)?;
                let mut arms = Vec::new();
                loop {
                    if self.eat(&Tok::RBrace) {
                        break;
                    }
                    if self.peek() == &Tok::Attr {
                        self.bump();
                        let mut d = 1usize;
                        while d > 0 {
                            match self.bump() {
                                Tok::Eof => return bad("eof"),
                                Tok::LBracket => d += 1,
                                Tok::RBracket => d -= 1,
                                _ => {}
                            }
                        }
                        continue;
                    }
                    let pat = self.pattern()?;
                    if self.peek() == &Tok::If {
                        return bad("match guard unsupported");
                    }
                    self.expect(Tok::FatArrow)?;
                    let body = self.expr()?;
                    arms.push(Arm { pats: vec![pat], body });
                    if !self.eat(&Tok::Comma) {
                        if self.peek() == &Tok::RBrace {
                            self.bump();
                            break;
                        }
                    }
                }
                Ok(Expr::Match(Box::new(scrut), arms))
            }
            Tok::Loop => Ok(Expr::Loop(Box::new(self.block_expr()?))),
            Tok::While => {
                let c = self.expr_no_lit()?;
                Ok(Expr::While(Box::new(c), Box::new(self.block_expr()?)))
            }
            Tok::For => {
                // for <pat> in <expr>
                let pat = self.pattern()?;
                self.expect(Tok::In)?;
                let it = self.expr_no_lit()?;
                Ok(Expr::For(pat, Box::new(it), Box::new(self.block_expr()?)))
            }
            Tok::Break => {
                let v = if !matches!(self.peek(), Tok::Semi | Tok::RBrace | Tok::Comma) {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                Ok(Expr::Break(v))
            }
            Tok::Continue => Ok(Expr::Continue),
            Tok::Return => {
                let v = if !matches!(self.peek(), Tok::Semi | Tok::RBrace | Tok::Comma) {
                    Some(Box::new(self.expr()?))
                } else {
                    None
                };
                Ok(Expr::Return(v))
            }
            Tok::Ident(n) => {
                let mut path = vec![n];
                while self.peek() == &Tok::ColonColon {
                    // path segment or struct literal Name{..} handled below
                    self.bump();
                    if self.peek() == &Tok::Lt {
                        self.skip_generic()?;
                        if self.peek() != &Tok::ColonColon {
                            break;
                        }
                        self.bump();
                    }
                    match self.bump() {
                        Tok::Ident(s) => path.push(s),
                        Tok::SelfValue => path.push("self".into()),
                        other => return bad(format!("bad path seg {other:?}")),
                    }
                }
                // struct literal Name { a: x, .. }
                if self.peek() == &Tok::LBrace && path.len() == 1 && !self.no_struct_lit {
                    self.bump();
                    let mut fields = Vec::new();
                    loop {
                        if self.eat(&Tok::RBrace) {
                            break;
                        }
                        if self.eat(&Tok::DotDot) {
                            let _ = self.expr()?;
                            self.eat(&Tok::Comma);
                            self.expect(Tok::RBrace)?;
                            break;
                        }
                        let n = self.ident()?;
                        let v = if self.eat(&Tok::Colon) {
                            self.expr()?
                        } else {
                            Expr::Var(n.clone())
                        };
                        fields.push((n, v));
                        if !self.eat(&Tok::Comma) {
                            self.expect(Tok::RBrace)?;
                            break;
                        }
                    }
                    return Ok(Expr::StructLit(path, fields));
                }
                if self.peek() == &Tok::Not {
                    // name!{..}: mark unsupported, skip the token tree
                    self.bump();
                    match self.peek().clone() {
                        Tok::LBrace => { self.skip_tree(Tok::LBrace, Tok::RBrace)?; }
                        Tok::LParen => { self.skip_tree(Tok::LParen, Tok::RParen)?; }
                        Tok::LBracket => { self.skip_tree(Tok::LBracket, Tok::RBracket)?; }
                        _ => {}
                    }
                    return Ok(Expr::Unsupported(format!("macro {path:?}!")));
                }
                Ok(if path.len() == 1 {
                    Expr::Var(path.pop().unwrap())
                } else {
                    Expr::Path(path)
                })
            }
            other => bad(format!("unexpected token {other:?}")),
        }
    }
}
