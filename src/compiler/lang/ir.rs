//! LLVM-IR text emission for the nasc subset. Type inference is minimal:
//! integer literals default to i32 and coerce at binding/call boundaries;
//! every unsupported construct returns Unsupported so the caller can fall
//! back to the rustc-bitcode patch path.

use crate::parse::{Arm, BinOp, Expr, FnItem, Pat, Stmt, Ty, Unsupported};
use std::collections::BTreeMap;
use std::fmt::Write;

type Res<T> = Result<T, Unsupported>;
fn bad<T>(m: impl Into<String>) -> Res<T> {
    Err(Unsupported::Msg(m.into()))
}

/// A declared symbol the impl may call: LLVM signature plus, when known,
/// the Rust path segments used for name resolution.
#[derive(Debug, Clone)]
pub struct Decl {
    pub llvm_name: String,
    pub ret: String,
    pub args: Vec<String>,
    /// Rust path segments (demangled, without hashes) when resolvable.
    pub rust_name: Option<String>,
}

/// A call site recorded verbatim from the base module: exact callee
/// symbol, LLVM signature (with sret/align attrs) and the demangled Rust
/// path for source-name resolution.
#[derive(Debug, Clone)]
pub struct CallDecl {
    pub llvm_name: String,
    /// Full LLVM param specs ("ptr sret([32 x i8]) align 8"), `;`-joined.
    pub params: Vec<String>,
    /// Return type text as it appeared on the call ("zeroext i1").
    pub ret: String,
    pub demangled: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Sig {
    pub triple: String,
    pub layout: String,
    /// `define <...> @stub(...)` line for the impl, verbatim minus name.
    pub define: String,
    pub ret: String,
    pub params: Vec<(String, String)>, // (llvm type, ir name)
    pub decls: Vec<Decl>,
    /// Rust name of the function to compile (from `want <name>`).
    pub want: Option<String>,
    /// Verbatim call sites from the base body, in instruction order.
    pub callsites: Vec<CallDecl>,
}

pub fn parse_sig(text: &str) -> Res<Sig> {
    let mut sig = Sig {
        triple: String::new(),
        layout: String::new(),
        define: String::new(),
        ret: String::new(),
        params: Vec::new(),
        decls: Vec::new(),
        want: None,
        callsites: Vec::new(),
    };
    for line in text.lines() {
        let mut parts = line.splitn(4, ' ');
        match parts.next() {
            Some("triple") => {
                sig.triple = parts.next().unwrap_or("").to_string();
            }
            Some("layout") => {
                sig.layout = parts.next().unwrap_or("").to_string();
            }
            Some("define") => {
                // `define <ret> @name(<args>) <attrs>`; keep the raw line.
                // Only the line whose @name ends with __ns_patch_impl_<i> is
                // the impl slot for this compile; other defines are decls.
                if !line.contains("@__ns_patch_impl") {
                    continue;
                }
                let rest = line.strip_prefix("define ").unwrap_or(line);
                let at = rest.find('@').ok_or_else(|| {
                    Unsupported::Msg("sig: no @name in define".into())
                })?;
                sig.ret = rest[..at].trim_end().to_string();
                let open = rest.find('(').ok_or_else(|| {
                    Unsupported::Msg("sig: no ( in define".into())
                })?;
                let close = rest.rfind(')').ok_or_else(|| {
                    Unsupported::Msg("sig: no ) in define".into())
                })?;
                let args: Vec<String> = rest[open + 1..close]
                    .split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
                sig.params = args
                    .into_iter()
                    .enumerate()
                    .map(|(i, a)| {
                        let ty = a.split_whitespace().next().unwrap_or(&a).to_string();
                        (ty, format!("%{i}"))
                    })
                    .collect();
                sig.define = line.to_string();
            }
            Some("want") => {
                sig.want = Some(parts.next().unwrap_or("").to_string());
            }
            Some("decl") => {
                // decl <llvm_name> <ret> <rust_name or -> <arg,arg,...>
                let name = parts.next().unwrap_or("").trim_start_matches('@').to_string();
                let ret = parts.next().unwrap_or("").to_string();
                let rest = parts.next().unwrap_or("");
                let (rust, args) = rest.split_once('|').unwrap_or((rest, ""));
                let rust = rust.trim();
                sig.decls.push(Decl {
                    llvm_name: name,
                    ret,
                    args: args
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                    rust_name: (rust != "-" && !rust.is_empty()).then(|| rust.to_string()),
                });
            }
            Some("call") => {
                // call <llvm_name> |<demangled or ->|<ret>|<param;param;...>
                // Pipe-delimited: demangled paths and param specs contain
                // spaces, params are `;`-joined.
                let rest = line.strip_prefix("call ").unwrap_or(line);
                let mut seg = rest.splitn(4, '|');
                let name = seg.next().unwrap_or("").trim().trim_start_matches('@').to_string();
                let dem = seg.next().unwrap_or("").trim();
                let ret = seg.next().unwrap_or("").trim().to_string();
                let args = seg.next().unwrap_or("");
                sig.callsites.push(CallDecl {
                    llvm_name: name,
                    params: args
                        .split(';')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect(),
                    ret,
                    demangled: (dem != "-" && !dem.is_empty()).then(|| dem.to_string()),
                });
            }
            _ => {}
        }
    }
    if sig.define.is_empty() {
        return bad("sig: missing define");
    }
    Ok(sig)
}

/// Demangled path minus generic args: strip `<...>` from every `::`
/// segment and drop empty segments, so
/// `Result::<T>::is_ok_and::<C>` -> `Result::is_ok_and` and
/// `crate::f::{closure#0}` -> `crate::f`.
fn base_path(dem: &str) -> String {
    // Drop `<...>` regions (depth-tracked) then split on `::` boundaries;
    // segments that were purely generic args vanish.
    let mut out = String::with_capacity(dem.len());
    let mut depth = 0usize;
    for c in dem.chars() {
        match c {
            '<' => depth += 1,
            '>' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.split("::")
        .filter(|seg| !seg.is_empty())
        .collect::<Vec<_>>()
        .join("::")
}

/// Resolve a source path/method name to a recorded call site. Exact path
/// wins over leaf-only matches; leaf must be a `::`-boundary suffix.
fn find_callsite<'a>(cs: &'a [CallDecl], name: &str) -> Option<&'a CallDecl> {
    let leaf = name.rsplit("::").next().unwrap_or(name);
    cs.iter()
        .filter(|d| {
            let Some(dem) = d.demangled.as_deref() else { return false };
            let base = base_path(dem);
            name.contains("::") && (base == name || base.ends_with(&format!("::{name}")))
        })
        .max_by_key(|d| base_path(d.demangled.as_deref().unwrap_or("")).len())
        .or_else(|| {
            let mut hits: Vec<&CallDecl> = cs
                .iter()
                .filter(|d| {
                    d.demangled.as_deref().is_some_and(|dem| {
                        let base = base_path(dem);
                        base == leaf || base.ends_with(&format!("::{leaf}"))
                    })
                })
                .collect();
            hits.sort_by_key(|d| base_path(d.demangled.as_deref().unwrap_or("")).len());
            hits.into_iter().next()
        })
        .or_else(|| {
            // Legacy manglings often refuse demangling; the leaf still
            // survives verbatim as a length-prefixed component, so
            // `9is_ok_and` inside the mangled name identifies the method.
            let needle = format!("{}{leaf}", leaf.len());
            cs.iter()
                .filter(|d| d.demangled.is_none() && d.llvm_name.contains(&needle))
                .min_by_key(|d| d.llvm_name.len())
        })
}

fn llty_of(name: &str) -> String {
    match name {
        "u8" => "i8".into(),
        "u16" => "i16".into(),
        "u32" => "i32".into(),
        "u64" | "usize" => "i64".into(),
        "u128" => "i128".into(),
        "isize" => "i64".into(),
        _ => name.to_string(),
    }
}

#[derive(Debug, Clone, PartialEq)]
enum V {
    Int(u8, bool),        // bits, signed
    Bool,
    Str,                  // (ptr, i64) fat
    Ptr,                  // opaque ptr
    /// Pointer to a value of unknown layout (sret allocas, by-ref args).
    /// Carries the pointee's LLVM type text for allocation/copies.
    Opaque(String),
    Unit,
}

impl V {
    fn llvm(&self) -> &'static str {
        match self {
            V::Int(8, true) => "i8",
            V::Int(16, true) => "i16",
            V::Int(32, true) => "i32",
            V::Int(64, true) => "i64",
            V::Int(128, true) => "i128",
            V::Int(8, false) => "i8",
            V::Int(16, false) => "i16",
            V::Int(32, false) => "i32",
            V::Int(64, false) => "i64",
            V::Int(128, false) => "i128",
            V::Int(_, _) => "i32",
            V::Bool => "i1",
            V::Str => "{ ptr, i64 }",
            V::Ptr => "ptr",
            V::Opaque(_) => "ptr",
            V::Unit => "void",
        }
    }
}

#[derive(Debug)]
struct Val {
    ty: V,
    /// register, constant literal, or for Str the pointer operand text.
    text: String,
    /// Str only: length operand text.
    len: Option<String>,
}

struct Emitter<'a> {
    out: String,
    decls_out: String,
    strs: Vec<String>,
    next: usize,
    vars: BTreeMap<String, (V, String)>, // name -> (ty, alloca reg)
    ret_v: V,
    labels: usize,
    loop_stack: Vec<(String, String)>, // (continue, break)
    sig: &'a Sig,
}

pub fn emit(f: &FnItem, sig: &Sig) -> Res<String> {
    // LLVM params consume %arg0..%argN in the define, so temporaries
    // must start above them or llvm-as rejects the numbering.
    let mut e = Emitter {
        out: String::new(),
        decls_out: String::new(),
        strs: Vec::new(),
        next: sig.params.len(),
        vars: BTreeMap::new(),
        ret_v: ret_ty(&f.ret)?,
        labels: 0,
        loop_stack: Vec::new(),
        sig,
    };
    // params: bind to allocas
    e.out.push_str("entry:\n");
    let mut params = Vec::new();
    let mut lp = 0usize;
    for (i, (pname, pty)) in f.params.iter().enumerate() {
        let v = param_ty(pty)?;
        let a = e.alloca(&v);
        if v == V::Str {
            // &str arrives as two llvm params (ptr, i64); fuse into the
            // fat alloca so loads split them uniformly.
            let p = sig.params.get(lp).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("%{lp}"));
            let l = sig.params.get(lp + 1).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("%{}", lp + 1));
            lp += 2;
            params.push(format!("ptr {p}"));
            params.push(format!("i64 {l}"));
            let x = e.reg();
            let y = e.reg();
            e.line(format!("{x} = insertvalue {{ ptr, i64 }} poison, ptr {p}, 0"));
            e.line(format!("{y} = insertvalue {{ ptr, i64 }} {x}, i64 {l}, 1"));
            e.line(format!("store {{ ptr, i64 }} {y}, ptr {a}"));
        } else {
            let reg = sig.params.get(lp).map(|(_, n)| n.clone()).unwrap_or_else(|| format!("%{lp}"));
            lp += 1;
            params.push(format!("{} {}", llvm_param(&v), reg));
            e.line(format!("store {} {}, ptr {}", v.llvm(), reg, a));
        }
        e.vars.insert(pname.clone(), (v, a));
        let _ = i;
    }
    let tail = e.expr(&f.body)?;
    e.ret(&tail)?;

    // assemble module
    let mut m = String::new();
    if !sig.triple.is_empty() {
        let _ = writeln!(m, "target triple = \"{}\"", sig.triple);
    }
    if !sig.layout.is_empty() {
        let _ = writeln!(m, "target datalayout = \"{}\"", sig.layout);
    }
    for (i, s) in e.strs.iter().enumerate() {
        let _ = writeln!(
            m,
            "@.nasc.str.{i} = private unnamed_addr constant [{} x i8] c\"{}\", align 1",
            s.len() + 1,
            escape_str(s)
        );
    }
    m.push_str(&e.decls_out);
    // define header: sig's define with impl name substituted and attrs kept
    // replace the whole @name token (may carry an index suffix)
    // keep the exact impl name from the sig (@__ns_patch_impl_<i>)
    let header = sig.define.clone();
    let _ = writeln!(m, "{header} {{");
    m.push_str(&e.out);
    m.push_str("}\n");
    Ok(m)
}



fn escape_str(s: &str) -> String {
    let mut o = String::new();
    for c in s.chars() {
        match c {
            '\n' => o.push_str("\\0A"),
            '\t' => o.push_str("\\09"),
            '\r' => o.push_str("\\0D"),
            '"' => o.push_str("\\22"),
            '\\' => o.push_str("\\5C"),
            c if (c as u32) < 0x20 || (c as u32) > 0x7e => {
                let _ = write!(o, "\\{:02X}", c as u32);
            }
            c => o.push(c),
        }
    }
    o
}

fn ret_ty(t: &Ty) -> Res<V> {
    Ok(match t {
        Ty::Unit => V::Unit,
        Ty::Bool => V::Bool,
        Ty::Int(name) => int_ty(name)?,
        Ty::Str => V::Str,
        Ty::Ref(_) => V::Ptr,
        Ty::Named(_) => return bad("named return type unsupported"),
        Ty::Tuple(t) if t.is_empty() => V::Unit,
        _ => return bad("return type unsupported"),
    })
}

fn param_ty(t: &Ty) -> Res<V> {
    Ok(match t {
        Ty::Bool => V::Bool,
        Ty::Int(name) => int_ty(name)?,
        Ty::Str => V::Str,
        Ty::Ref(_) => V::Ptr,
        Ty::Unit => V::Unit,
        _ => return bad("param type unsupported"),
    })
}

fn int_ty(name: &str) -> Res<V> {
    Ok(match name {
        "i8" => V::Int(8, true),
        "i16" => V::Int(16, true),
        "i32" => V::Int(32, true),
        "i64" | "isize" => V::Int(64, true),
        "i128" => V::Int(128, true),
        "u8" => V::Int(8, false),
        "u16" => V::Int(16, false),
        "u32" => V::Int(32, false),
        "u64" | "usize" => V::Int(64, false),
        "u128" => V::Int(128, false),
        _ => return bad(format!("int type {name}")),
    })
}

fn llvm_param(v: &V) -> String {
    match v {
        V::Str => "ptr, i64".to_string(),
        _ => v.llvm().to_string(),
    }
}

impl<'a> Emitter<'a> {
    fn line(&mut self, s: String) {
        let _ = writeln!(self.out, "  {s}");
    }
    fn reg(&mut self) -> String {
        self.next += 1;
        format!("%{}", self.next)
    }
    fn alloca(&mut self, v: &V) -> String {
        let r = self.reg();
        self.line(format!("{r} = alloca {}, align 8", v.llvm()));
        r
    }
    fn label(&mut self, hint: &str) -> String {
        self.labels += 1;
        format!("L{}", self.labels).replace('L', hint)
            + &self.labels.to_string()
    }
    fn br(&mut self, l: &str) {
        self.line(format!("br label %{l}"));
    }
    fn named(&mut self, l: &str) {
        let _ = writeln!(self.out, "{l}:");
    }

    fn ret(&mut self, v: &Val) -> Res<()> {
        match (&self.ret_v, &v.ty) {
            (V::Unit, _) => self.line("ret void".into()),
            (r, t) if r == t => self.line(format!("ret {} {}", r.llvm(), v.text)),
            (V::Int(rb, rs), V::Int(_tb, _ts)) => {
                let c = self.coerce(v, &V::Int(*rb, *rs))?;
                self.line(format!("ret {} {}", self.ret_v.llvm(), c.text));
            }
            (V::Bool, V::Bool) => self.line(format!("ret i1 {}", v.text)),
            (V::Ptr, V::Str) => {
                self.line(format!("ret ptr {}", v.text));
            }
            _ => return bad(format!("ret type {:?} vs {:?}", self.ret_v, v.ty)),
        }
        Ok(())
    }

    fn coerce(&mut self, v: &Val, want: &V) -> Res<Val> {
        if &v.ty == want {
            return Ok(v.clone_val());
        }
        match (&v.ty, want) {
            (V::Int(a, as_), V::Int(b, bs)) => {
                if a == b {
                    return Ok(Val { ty: want.clone(), text: v.text.clone(), len: None });
                }
                let op = if a < b {
                    if *as_ { "sext" } else { "zext" }
                } else {
                    "trunc"
                };
                let _ = (as_, bs);
                let r = self.reg();
                self.line(format!(
                    "{r} = {op} {} {} to {}",
                    v.ty.llvm(),
                    v.text,
                    want.llvm()
                ));
                Ok(Val { ty: want.clone(), text: r, len: None })
            }
            (V::Str, V::Ptr) => Ok(Val { ty: V::Ptr, text: v.text.clone(), len: None }),
            _ => bad(format!("cannot coerce {:?} to {:?}", v.ty, want)),
        }
    }

    fn load(&mut self, name: &str) -> Res<Val> {
        let Some((v, ptr)) = self.vars.get(name) else {
            return bad(format!("unbound var {name}"));
        };
        let (v, ptr) = (v.clone(), ptr.clone());
        if v == V::Str {
            // &str alloca stores the fat aggregate; split for callers.
            let a = self.reg();
            let p = self.reg();
            let l = self.reg();
            self.line(format!("{a} = load {{ ptr, i64 }}, ptr {ptr}"));
            self.line(format!("{p} = extractvalue {{ ptr, i64 }} {a}, 0"));
            self.line(format!("{l} = extractvalue {{ ptr, i64 }} {a}, 1"));
            return Ok(Val { ty: V::Str, text: p, len: Some(l) });
        }
        let r = self.reg();
        self.line(format!("{r} = load {}, ptr {ptr}", v.llvm()));
        Ok(Val { ty: v, text: r, len: None })
    }

    fn expr(&mut self, e: &Expr) -> Res<Val> {
        match e {
            Expr::Int(n) => Ok(Val {
                ty: V::Int(32, true),
                text: n.to_string(),
                len: None,
            }),
            Expr::Bool(b) => Ok(Val {
                ty: V::Bool,
                text: if *b { "1" } else { "0" }.to_string(),
                len: None,
            }),
            Expr::Char(c) => Ok(Val {
                ty: V::Int(32, false),
                text: (*c as u32).to_string(),
                len: None,
            }),
            Expr::Unit => Ok(Val { ty: V::Unit, text: String::new(), len: None }),
            Expr::Str(s) => {
                let idx = self.strs.iter().position(|x| x == s).unwrap_or_else(|| {
                    self.strs.push(s.clone());
                    self.strs.len() - 1
                });
                Ok(Val {
                    ty: V::Str,
                    text: format!("@.nasc.str.{idx}"),
                    len: Some(s.len().to_string()),
                })
            }
            Expr::Var(n) => self.load(n),
            Expr::Path(p) => {
                let n = p.join("::");
                if let Some(d) = self.sig.decls.iter().find(|d| {
                    d.rust_name.as_deref() == Some(n.as_str())
                        || d.rust_name.as_deref() == Some(p.last().unwrap().as_str())
                }) {
                    Ok(Val { ty: V::Ptr, text: format!("@{}", d.llvm_name), len: None })
                } else {
                    bad(format!("unresolvable path {n}"))
                }
            }
            Expr::Ref(inner, mutable) => {
                let _ = mutable;
                match inner.as_ref() {
                    Expr::Var(n) => {
                        let Some((v, ptr)) = self.vars.get(n) else {
                            return bad(format!("unbound var {n}"));
                        };
                        Ok(Val { ty: V::Ptr, text: ptr.clone(), len: None })
                            .map(|x| { let _ = v; x })
                    }
                    Expr::Str(_) => self.expr(inner),
                    _ => bad("&expr unsupported"),
                }
            }
            Expr::Unary(op, inner) => {
                let v = self.expr(inner)?;
                match (op, &v.ty) {
                    (crate::parse::UnOp::Neg, V::Int(..)) => {
                        let r = self.reg();
                        self.line(format!("{r} = sub {} 0, {}", v.ty.llvm(), v.text));
                        Ok(Val { ty: v.ty, text: r, len: None })
                    }
                    (crate::parse::UnOp::Not, V::Bool) => {
                        let r = self.reg();
                        self.line(format!("{r} = xor i1 {}, 1", v.text));
                        Ok(Val { ty: V::Bool, text: r, len: None })
                    }
                    (crate::parse::UnOp::Not, V::Int(..)) => {
                        let r = self.reg();
                        self.line(format!("{r} = xor {} {}, -1", v.ty.llvm(), v.text));
                        Ok(Val { ty: v.ty, text: r, len: None })
                    }
                    _ => bad("unary operand type unsupported"),
                }
            }
            Expr::Bin(op, l, r) => self.bin(*op, l, r),
            Expr::If(c, t, f) => self.if_expr(c, t, f.as_deref()),
            Expr::Block(stmts, tail) => self.block(stmts, tail.as_deref()),
            Expr::Return(v) => {
                let val = match v {
                    Some(e) => self.expr(e)?,
                    None => Val { ty: V::Unit, text: String::new(), len: None },
                };
                self.ret(&val)?;
                let dead = self.label("dead");
                self.br(&dead);
                self.named(&dead);
                Ok(Val { ty: V::Unit, text: String::new(), len: None })
            }
            Expr::Call(callee, args) => self.call(callee, args),
            Expr::Method(recv, name, args) => self.call_method(recv, name, args),
            Expr::As(e, t) => {
                let v = self.expr(e)?;
                let want = match t {
                    Ty::Int(name) => *name,
                    Ty::Named(n) => n.as_str(),
                    _ => "",
                };
                match want {
                    "i8" | "i16" | "i32" | "i64" | "i128" | "u8" | "u16" | "u32" | "u64" | "u128" | "isize" | "usize" => {
                        self.cast_to_llty(&v, llty_of(want))
                    }
                    _ => bad::<Val>(format!("as {want}")),
                }
            }
            Expr::Unsupported(what) => bad(format!("unsupported {what}")),
            Expr::Assign(l, r) => self.assign(l, r),
            Expr::AssignOp(op, l, r) => {
                let cur = match l.as_ref() {
                    Expr::Var(n) => self.load(n)?,
                    _ => return bad("assign-op target unsupported"),
                };
                let rhs = self.expr(r)?;
                let rhs = self.coerce(&rhs, &cur.ty)?;
                let t = cur.ty.clone();
                let inst = match op {
                    BinOp::Add => "add",
                    BinOp::Sub => "sub",
                    BinOp::Mul => "mul",
                    BinOp::Div => "sdiv",
                    BinOp::Rem => "srem",
                    BinOp::BitAnd => "and",
                    BinOp::BitOr => "or",
                    BinOp::BitXor => "xor",
                    BinOp::Shl => "shl",
                    BinOp::Shr => "lshr",
                    _ => return bad("bad assign op"),
                };
                let nr = self.reg();
                self.line(format!(
                    "{nr} = {inst} {} {}, {}",
                    t.llvm(),
                    cur.text,
                    rhs.text
                ));
                self.store_to(l, &Val { ty: t, text: nr, len: None })?;
                Ok(Val { ty: V::Unit, text: String::new(), len: None })
            }
            Expr::While(c, b) => self.while_expr(c, b),
            Expr::Loop(b) => self.loop_expr(b),
            Expr::For(p, it, b) => self.for_expr(p, it, b),
            Expr::Break(v) => {
                let Some((_, bl)) = self.loop_stack.last() else {
                    return bad("break outside loop");
                };
                if v.is_some() {
                    return bad("break with value unsupported");
                }
                let l = bl.clone();
                self.br(&l);
                let dead = self.label("dead");
                self.named(&dead);
                Ok(Val { ty: V::Unit, text: String::new(), len: None })
            }
            Expr::Continue => {
                let Some((cl, _)) = self.loop_stack.last() else {
                    return bad("continue outside loop");
                };
                let l = cl.clone();
                self.br(&l);
                let dead = self.label("dead");
                self.named(&dead);
                Ok(Val { ty: V::Unit, text: String::new(), len: None })
            }
            Expr::Match(s, arms) => self.match_expr(s, arms),
            Expr::As(e, t) => {
                let v = self.expr(e)?;
                let want = param_ty(t)?;
                self.coerce(&v, &want)
            }
            _ => bad(format!("unsupported expr {e:?}")),
        }
    }

    fn store_to(&mut self, l: &Expr, v: &Val) -> Res<()> {
        match l {
            Expr::Var(n) => {
                let Some((vt, ptr)) = self.vars.get(n) else {
                    return bad(format!("unbound var {n}"));
                };
                let (vt, ptr) = (vt.clone(), ptr.clone());
                let c = self.coerce(v, &vt)?;
                if vt == V::Str {
                    let a = self.reg();
                    let b = self.reg();
                    self.line(format!("{a} = insertvalue {{ ptr, i64 }} poison, ptr {}, 0", c.text));
                    self.line(format!("{b} = insertvalue {{ ptr, i64 }} {a}, i64 {}, 1", c.len.clone().unwrap_or_default()));
                    self.line(format!("store {{ ptr, i64 }} {b}, ptr {ptr}"));
                    return Ok(());
                }
                self.line(format!("store {} {}, ptr {ptr}", vt.llvm(), c.text));
                Ok(())
            }
            _ => bad("assign target unsupported"),
        }
    }

    fn cast_int(&mut self, v: &Val, w: u8) -> Res<Val> {
        self.cast_to_llty(v, format!("i{w}"))
    }

    fn cast_to_llty(&mut self, v: &Val, want: String) -> Res<Val> {
        match (&v.ty, want.as_str()) {
            (V::Int(f, fs), w) if w.starts_with('i') && w[1..].parse::<u16>().is_ok() => {
                let tw = w[1..].parse::<u8>().unwrap();
                if tw == *f {
                    Ok(v.clone_val())
                } else {
                    let r = self.reg();
                    let op = if tw > *f { "sext" } else { "trunc" };
                    // unsigned sources still sign-extend fine for rustc semantics here
                    let _ = fs;
                    self.line(format!("{r} = {op} i{f} {} to {w}", v.text));
                    Ok(Val { ty: V::Int(tw, true), text: r, len: None })
                }
            }
            (V::Ptr, w) if w.starts_with('i') => {
                let r = self.reg();
                self.line(format!("{r} = ptrtoint ptr {} to {w}", v.text));
                Ok(Val { ty: V::Int(w[1..].parse().unwrap_or(64), false), text: r, len: None })
            }
            _ => bad::<Val>(format!("cast {:?} -> {want}", v.ty)),
        }
    }

    fn assign(&mut self, l: &Expr, r: &Expr) -> Res<Val> {
        let v = self.expr(r)?;
        self.store_to(l, &v)?;
        Ok(Val { ty: V::Unit, text: String::new(), len: None })
    }

    fn bin(&mut self, op: BinOp, l: &Expr, r: &Expr) -> Res<Val> {
        if op == BinOp::And || op == BinOp::Or {
            return self.short_circuit(op, l, r);
        }
        let lv = self.expr(l)?;
        let rv = self.expr(r)?;
        let rt = widest(&lv.ty, &rv.ty)?;
        let lv = self.coerce(&lv, &rt)?;
        let rv = self.coerce(&rv, &rt)?;
        let (inst, ty) = match op {
            BinOp::Add => ("add", rt.clone()),
            BinOp::Sub => ("sub", rt.clone()),
            BinOp::Mul => ("mul", rt.clone()),
            BinOp::Div => ("sdiv", rt.clone()),
            BinOp::Rem => ("srem", rt.clone()),
            BinOp::BitAnd => ("and", rt.clone()),
            BinOp::BitOr => ("or", rt.clone()),
            BinOp::BitXor => ("xor", rt.clone()),
            BinOp::Shl => ("shl", rt.clone()),
            BinOp::Shr => ("lshr", rt.clone()),
            BinOp::Eq => return self.icmp("eq", &lv, &rv),
            BinOp::Ne => return self.icmp("ne", &lv, &rv),
            BinOp::Lt => return self.icmp("slt", &lv, &rv),
            BinOp::Le => return self.icmp("sle", &lv, &rv),
            BinOp::Gt => return self.icmp("sgt", &lv, &rv),
            BinOp::Ge => return self.icmp("sge", &lv, &rv),
            _ => return bad("bad binop"),
        };
        let reg = self.reg();
        self.line(format!("{reg} = {inst} {} {}, {}", rt.llvm(), lv.text, rv.text));
        Ok(Val { ty, text: reg, len: None })
    }

    fn icmp(&mut self, pred: &str, l: &Val, r: &Val) -> Res<Val> {
        let reg = self.reg();
        self.line(format!(
            "{reg} = icmp {pred} {} {}, {}",
            l.ty.llvm(),
            l.text,
            r.text
        ));
        Ok(Val { ty: V::Bool, text: reg, len: None })
    }

    fn short_circuit(&mut self, op: BinOp, l: &Expr, r: &Expr) -> Res<Val> {
        let lv = self.expr(l)?;
        let res = self.alloca(&V::Bool);
        let rhs_l = self.label("sc_rhs");
        let done_l = self.label("sc_done");
        let cond = match op {
            BinOp::And => format!("br i1 {}, label %{rhs_l}, label %{done_l}", lv.text),
            _ => format!("br i1 {}, label %{done_l}, label %{rhs_l}", lv.text),
        };
        self.line(format!("store i1 {}, ptr {res}", lv.text));
        self.line(cond);
        self.named(&rhs_l);
        let rv = self.expr(r)?;
        let rv = self.coerce(&rv, &V::Bool)?;
        self.line(format!("store i1 {}, ptr {res}", rv.text));
        self.br(&done_l);
        self.named(&done_l);
        let reg = self.reg();
        self.line(format!("{reg} = load i1, ptr {res}"));
        Ok(Val { ty: V::Bool, text: reg, len: None })
    }

    fn if_expr(&mut self, c: &Expr, t: &Expr, f: Option<&Expr>) -> Res<Val> {
        let cv = self.expr(c)?;
        let cv = self.coerce(&cv, &V::Bool)?;
        let t_l = self.label("if_t");
        let f_l = self.label("if_f");
        let d_l = self.label("if_done");
        self.line(format!("br i1 {}, label %{t_l}, label %{f_l}", cv.text));
        // value-only if both arms produce same non-unit type
        self.named(&t_l);
        let tv = self.expr(t)?;
        let has_val = tv.ty != V::Unit && f.is_some();
        let slot = if has_val { Some(self.alloca(&tv.ty)) } else { None };
        if let Some(p) = &slot {
            self.line(format!("store {} {}, ptr {p}", tv.ty.llvm(), tv.text));
        }
        self.br(&d_l);
        self.named(&f_l);
        if let Some(fe) = f {
            let fv = self.expr(fe)?;
            if let Some(p) = &slot {
                let fv = self.coerce(&fv, &tv.ty)?;
                self.line(format!("store {} {}, ptr {p}", tv.ty.llvm(), fv.text));
            }
        }
        self.br(&d_l);
        self.named(&d_l);
        match slot {
            Some(p) => {
                let reg = self.reg();
                self.line(format!("{reg} = load {}, ptr {p}", tv.ty.llvm()));
                Ok(Val { ty: tv.ty, text: reg, len: None })
            }
            None => Ok(Val { ty: V::Unit, text: String::new(), len: None }),
        }
    }

    fn while_expr(&mut self, c: &Expr, b: &Expr) -> Res<Val> {
        let c_l = self.label("while_c");
        let b_l = self.label("while_b");
        let d_l = self.label("while_d");
        self.br(&c_l);
        self.named(&c_l);
        let cv = self.expr(c)?;
        let cv = self.coerce(&cv, &V::Bool)?;
        self.line(format!("br i1 {}, label %{b_l}, label %{d_l}", cv.text));
        self.named(&b_l);
        self.loop_stack.push((c_l.clone(), d_l.clone()));
        self.expr(b)?;
        self.loop_stack.pop();
        self.br(&c_l);
        self.named(&d_l);
        Ok(Val { ty: V::Unit, text: String::new(), len: None })
    }

    fn loop_expr(&mut self, b: &Expr) -> Res<Val> {
        let b_l = self.label("loop_b");
        let d_l = self.label("loop_d");
        self.br(&b_l);
        self.named(&b_l);
        self.loop_stack.push((b_l.clone(), d_l.clone()));
        self.expr(b)?;
        self.loop_stack.pop();
        self.br(&b_l);
        self.named(&d_l);
        Ok(Val { ty: V::Unit, text: String::new(), len: None })
    }

    fn for_expr(&mut self, p: &Pat, it: &Expr, b: &Expr) -> Res<Val> {
        let (lo, hi, incl) = match it {
            Expr::Range(l, h, i) => (l.as_ref(), h.as_ref(), *i),
            _ => return bad("for-in requires a range"),
        };
        let name = match p {
            Pat::Ident(n) => n.clone(),
            _ => return bad("for pattern unsupported"),
        };
        let lv = self.expr(lo)?;
        let hv = self.expr(hi)?;
        let it_ty = widest(&lv.ty, &hv.ty)?;
        let lv = self.coerce(&lv, &it_ty)?;
        let hv = self.coerce(&hv, &it_ty)?;
        let var = self.alloca(&it_ty);
        self.line(format!("store {} {}, ptr {var}", it_ty.llvm(), lv.text));
        self.vars.insert(name.clone(), (it_ty.clone(), var.clone()));
        let c_l = self.label("for_c");
        let b_l = self.label("for_b");
        let d_l = self.label("for_d");
        self.br(&c_l);
        self.named(&c_l);
        let cur = self.load(&name)?;
        let cmp = self.reg();
        let pred = if incl { "sle" } else { "slt" };
        self.line(format!(
            "{cmp} = icmp {pred} {} {}, {}",
            it_ty.llvm(),
            cur.text,
            hv.text
        ));
        self.line(format!("br i1 {cmp}, label %{b_l}, label %{d_l}"));
        self.named(&b_l);
        let inc_l = self.label("for_inc");
        self.loop_stack.push((inc_l.clone(), d_l.clone()));
        self.expr(b)?;
        self.loop_stack.pop();
        self.br(&inc_l);
        self.named(&inc_l);
        let cur2 = self.load(&name)?;
        let next = self.reg();
        self.line(format!("{next} = add {} {}, 1", it_ty.llvm(), cur2.text));
        self.line(format!("store {} {next}, ptr {var}", it_ty.llvm()));
        self.br(&c_l);
        self.named(&d_l);
        Ok(Val { ty: V::Unit, text: String::new(), len: None })
    }

    fn match_expr(&mut self, s: &Expr, arms: &[Arm]) -> Res<Val> {
        let sv = self.expr(s)?;
        let mut cases = Vec::new();
        let mut default = None;
        for arm in arms {
            for p in &arm.pats {
                match p {
                    Pat::Int(v) => cases.push((*v, arm)),
                    Pat::Bool(b) => cases.push((u128::from(*b), arm)),
                    Pat::Char(c) => cases.push((*c as u128, arm)),
                    Pat::Wild => default = Some(arm),
                    Pat::Or(ps) => {
                        for pp in ps {
                            match pp {
                                Pat::Int(v) => cases.push((*v, arm)),
                                Pat::Bool(b) => cases.push((u128::from(*b), arm)),
                                Pat::Char(c) => cases.push((*c as u128, arm)),
                                Pat::Wild => default = Some(arm),
                                _ => return bad("match pattern unsupported"),
                            }
                        }
                    }
                    _ => return bad("match pattern unsupported"),
                }
            }
        }
        let d_l = self.label("match_d");
        let default_l = self.label("match_default");
        let mut sw = format!(
            "switch {} {}, label %{default_l} [",
            sv.ty.llvm(),
            sv.text
        );
        let mut lbls = Vec::new();
        for (v, arm) in &cases {
            let l = self.label("case");
            lbls.push((l.clone(), *arm));
            let _ = write!(sw, "\n    {} {v}, label %{l}", sv.ty.llvm());
        }
        sw.push_str("\n  ]");
        self.line(sw);
        let mut result: Option<(V, String)> = None;
        for (i, (l, arm)) in lbls.iter().enumerate() {
            self.named(l);
            let av = self.expr(&arm.body)?;
            if i == 0 && av.ty != V::Unit {
                let p = self.alloca(&av.ty);
                result = Some((av.ty.clone(), p));
            }
            if let Some((t, p)) = &result {
                let av = self.coerce(&av, t)?;
                self.line(format!("store {} {}, ptr {p}", t.llvm(), av.text));
            }
            self.br(&d_l);
        }
        self.named(&default_l);
        match default {
            Some(arm) => {
                let av = self.expr(&arm.body)?;
                if let Some((t, p)) = &result {
                    let av = self.coerce(&av, t)?;
                    self.line(format!("store {} {}, ptr {p}", t.llvm(), av.text));
                }
            }
            None => {
                self.line("unreachable".into());
            }
        }
        self.br(&d_l);
        self.named(&d_l);
        match result {
            Some((t, p)) => {
                let r = self.reg();
                self.line(format!("{r} = load {}, ptr {p}", t.llvm()));
                Ok(Val { ty: t, text: r, len: None })
            }
            None => Ok(Val { ty: V::Unit, text: String::new(), len: None }),
        }
    }

    fn block(&mut self, stmts: &[Stmt], tail: Option<&Expr>) -> Res<Val> {
        for s in stmts {
            match s {
                Stmt::Let(pat, ty, init) => {
                    let (name, mutable) = match pat {
                        Pat::Ident(n) => {
                            let (n, m) = n
                                .strip_prefix("mut ")
                                .map(|x| (x.to_string(), true))
                                .unwrap_or((n.clone(), false));
                            (n, m)
                        }
                        Pat::Wild => {
                            if let Some(i) = init {
                                self.expr(i)?;
                            }
                            continue;
                        }
                        _ => return bad("let pattern unsupported"),
                    };
                    let _ = mutable;
                    let (v, val) = match (ty, init) {
                        (Some(t), Some(i)) => {
                            let want = param_ty(t)?;
                            let val = self.expr(i)?;
                            (want.clone(), self.coerce(&val, &want)?)
                        }
                        (Some(t), None) => (param_ty(t)?, {
                            let z = Val {
                                ty: param_ty(t)?,
                                text: "0".into(),
                                len: None,
                            };
                            z
                        }),
                        (None, Some(i)) => {
                            let val = self.expr(i)?;
                            (val.ty.clone(), val)
                        }
                        (None, None) => return bad("let without type or init"),
                    };
                    let a = self.alloca(&v);
                    self.line(format!("store {} {}, ptr {a}", v.llvm(), val.text));
                    self.vars.insert(name.clone(), (v, a));
                }
                Stmt::Expr(e) | Stmt::Semi(e) => {
                    self.expr(e)?;
                }
                Stmt::Item => {}
            }
        }
        match tail {
            Some(e) => self.expr(e),
            None => Ok(Val { ty: V::Unit, text: String::new(), len: None }),
        }
    }

    /// Emit one param spec `ptr sret([N x i8]) align 8` for a value;
    /// returns the spec prefix (attrs) to re-emit verbatim.
    fn param_head(spec: &str) -> &str {
        let s = spec.trim();
        let mut end = s.len();
        let mut depth = 0usize;
        for (i, c) in s.char_indices() {
            match c {
                '{' | '[' => depth += 1,
                '}' | ']' => depth = depth.saturating_sub(1),
                ' ' if depth == 0 => {
                    end = i;
                    break;
                }
                _ => {}
            }
        }
        &s[..end]
    }

    /// Evaluate the next source arg for one recorded param. Closures and
    /// `&mut _` capture synthetics are skipped when the param list is
    /// already exhausted for them.
    fn push_arg(&mut self, mut spec: &str, args: &[Expr], arg_i: &mut usize, next_spec: Option<&str>) -> Res<Val> {
        // Aggregate params for &str take the fat value directly.
        let head = Emitter::param_head(spec);
        let v = self.expr(&args[*arg_i])?;
        *arg_i += 1;
        match (&v.ty, head) {
            (V::Str, "{ ptr, i64 }") => {
                spec = "";
                let a = self.reg();
                let b = self.reg();
                self.line(format!("{a} = insertvalue {{ ptr, i64 }} poison, ptr {}, 0", v.text));
                self.line(format!("{b} = insertvalue {{ ptr, i64 }} {a}, i64 {}, 1", v.len.clone().unwrap_or_default()));
                Ok(Val { ty: V::Str, text: b, len: None })
            }
            (V::Str, "ptr") => {
                let _ = spec;
                // &str as (ptr, i64): caller emits the len slot next.
                Ok(v)
            }
            (V::Opaque(ty), "ptr") => {
                let _ = ty;
                Ok(v)
            }
            (_, "i64") => self.coerce(&v, &V::Int(64, true)),
            (_, "i32") => self.coerce(&v, &V::Int(32, true)),
            (_, "i16") => self.coerce(&v, &V::Int(16, true)),
            (_, "i8") => self.coerce(&v, &V::Int(8, true)),
            (_, "i128") => self.coerce(&v, &V::Int(128, true)),
            (_, "i1") => self.coerce(&v, &V::Bool),
            (_, "ptr") => self.coerce(&v, &V::Ptr),
            (t, head) => bad(format!("param head {head} for {t:?}")),
        }
    }

    /// Emit a resolved call site. `extra_args` are source args *before* the
    /// recorded params' remaining args (method receivers splice in front).
    fn emit_callsite(&mut self, cd: &CallDecl, src_args: &[Expr]) -> Res<Val> {
        if !self.decls_out.contains(&format!("@{}", cd.llvm_name)) {
            let _ = writeln!(
                self.decls_out,
                "declare {} @{}({})",
                cd.ret.trim_start_matches(|c: char| !c.is_ascii_alphabetic() && c != '{'),
                cd.llvm_name,
                cd.params.join(", ")
            );
        }
        let mut argtext = Vec::new();
        let mut ret_opaque: Option<String> = None;
        let mut pi = 0usize;
        let mut ai = 0usize;
        while pi < cd.params.len() {
            let spec = cd.params[pi].clone();
            if spec.contains("sret(") {
                // out-param: alloca of the sret pointee type.
                // `ptr sret([32 x i8]) align 8` -> `[32 x i8]`
                let inner = spec
                    .split_once("sret(")
                    .map(|(_, r)| {
                        let r = r.split(')').next().unwrap_or(r);
                        r.to_string()
                    })
                    .unwrap_or_default();
                let a = self.reg();
                self.line(format!("{a} = alloca {inner}, align 8"));
                argtext.push(format!("{spec} {a}"));
                ret_opaque = Some(inner);
                pi += 1;
                continue;
            }
            let head = Emitter::param_head(&spec).to_string();
            // skip source args that are closures or unrepresentable while
            // params remain (ZST args never materialize in LLVM IR).
            while ai < src_args.len() {
                match &src_args[ai] {
                    Expr::Closure(..) => {
                        ai += 1;
                    }
                    _ => break,
                }
            }
            if ai >= src_args.len() {
                return bad(format!(
                    "callsite @{}: out of args at param {}",
                    cd.llvm_name, pi
                ));
            }
            let v = if head == "ptr" && spec.contains("align") {
                // pointer arg to an alloca'd value (Result/String moves)
                let source = self.expr(&src_args[ai])?;
                ai += 1;
                match &source.ty {
                    V::Opaque(_) | V::Ptr => source,
                    _ => return bad(format!("align-ptr param from {source:?}")),
                }
            } else {
                self.push_arg(&spec, src_args, &mut ai, cd.params.get(pi + 1).map(|s| s.as_str()))?
            };
            argtext.push(format!("{spec} {}", v.text));
            // &str (ptr + i64 pair): emit the length into the next slot.
            if v.ty == V::Str && head == "ptr" {
                if let Some(next) = cd.params.get(pi + 1) {
                    if Emitter::param_head(next) == "i64" {
                        argtext.push(format!(
                            "{} {}",
                            next,
                            v.len.clone().unwrap_or_default()
                        ));
                        pi += 1;
                    }
                }
            }
            pi += 1;
        }
        // trailing source args may only be skipped closures (ZST args
        // never materialize in the LLVM signature).
        for a in &src_args[ai.min(src_args.len())..] {
            if !matches!(a, Expr::Closure(..)) {
                return bad(format!("callsite @{}: too many args", cd.llvm_name));
            }
        }
        let retbase = cd.ret.trim();
        let (flags, retty) = if retbase.starts_with("zeroext ") {
            ("zeroext ", &retbase["zeroext ".len()..])
        } else if retbase.starts_with("signext ") {
            ("signext ", &retbase["signext ".len()..])
        } else {
            ("", retbase)
        };
        let ret_v = match retty {
            "void" => V::Unit,
            "i1" => V::Bool,
            "i8" => V::Int(8, true),
            "i16" => V::Int(16, true),
            "i32" => V::Int(32, true),
            "i64" => V::Int(64, true),
            "i128" => V::Int(128, true),
            "ptr" => V::Ptr,
            "{ ptr, i64 }" => V::Str,
            _ => return bad(format!("callsite ret {retty}")),
        };
        if ret_opaque.is_some() && ret_v == V::Unit {
            self.line(format!(
                "call void @{}({})",
                cd.llvm_name,
                argtext.join(", ")
            ));
            return Ok(Val {
                ty: V::Opaque(ret_opaque.unwrap()),
                text: argtext[0]
                    .rsplit(' ')
                    .next()
                    .unwrap_or_default()
                    .to_string(),
                len: None,
            });
        }
        if ret_v == V::Unit {
            self.line(format!(
                "call void @{}({})",
                cd.llvm_name,
                argtext.join(", ")
            ));
            Ok(Val { ty: V::Unit, text: String::new(), len: None })
        } else {
            let r = self.reg();
            if retty == "{ ptr, i64 }" {
                // return pair registers for &str callers
                self.line(format!(
                    "{r} = call {flags}{{ ptr, i64 }} @{}({})",
                    cd.llvm_name,
                    argtext.join(", ")
                ));
                let p = self.reg();
                let l = self.reg();
                self.line(format!("{p} = extractvalue {{ ptr, i64 }} {r}, 0"));
                self.line(format!("{l} = extractvalue {{ ptr, i64 }} {r}, 1"));
                return Ok(Val { ty: V::Str, text: p, len: Some(l) });
            }
            self.line(format!(
                "{r} = call {flags}{retty} @{}({})",
                cd.llvm_name,
                argtext.join(", ")
            ));
            Ok(Val { ty: ret_v, text: r, len: None })
        }
    }

    fn call_method(&mut self, recv: &Expr, name: &str, args: &[Expr]) -> Res<Val> {
        let cd = find_callsite(&self.sig.callsites, name)
            .cloned()
            .ok_or_else(|| Unsupported::Msg(format!("no callsite for .{name}")))?;
        let mut src_args = Vec::with_capacity(args.len() + 1);
        src_args.push((*recv).clone());
        src_args.extend_from_slice(args);
        self.emit_callsite(&cd, &src_args)
    }

    fn call(&mut self, callee: &Expr, args: &[Expr]) -> Res<Val> {
        let name = match callee {
            Expr::Var(n) => n.clone(),
            Expr::Path(p) => p.join("::"),
            _ => return bad("indirect call unsupported"),
        };
        // Verbatim reissue: a recorded base call site is the strongest
        // evidence — exact symbol, sret/align/attrs preserved.
        if let Some(cd) = find_callsite(&self.sig.callsites, &name).cloned() {
            return self.emit_callsite(&cd, args);
        }
        let decl = self
            .sig
            .decls
            .iter()
            .find(|d| {
                let r = match d.rust_name.as_deref() {
                    Some(r) => r,
                    None => return false,
                };
                r == name
                    || r.rsplit("::").next() == Some(name.as_str())
                    || r.ends_with(&format!("::{name}"))
                    // `super::x`/`self::x`/`crate::x` resolve to leaf `x`
                    || name
                        .rsplit("::")
                        .next()
                        .is_some_and(|leaf| leaf != "self" && leaf != "super" && leaf != "crate" && r.rsplit("::").next() == Some(leaf))
            })
            .ok_or_else(|| Unsupported::Msg(format!("no decl for {name}")))?;
        // emit the decl once
        if !self.decls_out.contains(&format!("@{}", decl.llvm_name)) {
            let _ = writeln!(
                self.decls_out,
                "declare {} @{}({})",
                decl.ret,
                decl.llvm_name,
                decl.args.join(", ")
            );
        }
        let mut argtext = Vec::new();
        let mut ai = 0usize;
        for a in args {
            let v = self.expr(a)?;
            match (&v.ty, decl.args.get(ai).map(|s| s.as_str())) {
                (V::Str, Some("ptr")) => {
                    argtext.push(format!("ptr {}", v.text));
                    ai += 1;
                    if decl.args.get(ai).is_some_and(|t| t == "i64") {
                        argtext.push(format!("i64 {}", v.len.clone().unwrap_or_default()));
                        ai += 1;
                    }
                }
                (t, Some(sigty)) => {
                    let want = match sigty {
                        "ptr" => V::Ptr,
                        "i1" => V::Bool,
                        "i8" => V::Int(8, true),
                        "i16" => V::Int(16, true),
                        "i32" => V::Int(32, true),
                        "i64" => V::Int(64, true),
                        "i128" => V::Int(128, true),
                        _ => return bad(format!("arg type {sigty}")),
                    };
                    let c = self.coerce(&v, &want)?;
                    let _ = t;
                    argtext.push(format!("{sigty} {}", c.text));
                    ai += 1;
                }
                (_, None) => return bad("too many args"),
            }
        }
        if ai != decl.args.len() {
            return bad(format!(
                "arg count: {} llvm params, got {}",
                decl.args.len(),
                ai
            ));
        }
        let ret_v = match decl.ret.as_str() {
            "void" => V::Unit,
            "i1" => V::Bool,
            "i8" => V::Int(8, true),
            "i16" => V::Int(16, true),
            "i32" => V::Int(32, true),
            "i64" => V::Int(64, true),
            "i128" => V::Int(128, true),
            "ptr" => V::Ptr,
            other => return bad(format!("decl ret {other}")),
        };
        if ret_v == V::Unit {
            self.line(format!(
                "call void @{}({})",
                decl.llvm_name,
                argtext.join(", ")
            ));
            Ok(Val { ty: V::Unit, text: String::new(), len: None })
        } else {
            let r = self.reg();
            self.line(format!(
                "{r} = call {} @{}({})",
                decl.ret,
                decl.llvm_name,
                argtext.join(", ")
            ));
            Ok(Val { ty: ret_v, text: r, len: None })
        }
    }
}


impl Val {
    fn clone_val(&self) -> Val {
        Val { ty: self.ty.clone(), text: self.text.clone(), len: self.len.clone() }
    }
}

/// Widest of two operand types for binary arithmetic: usize/isize land on
/// i64; mismatched ints promote to the wider side.
fn widest(l: &V, r: &V) -> Res<V> {
    match (l, r) {
        (V::Int(a, sa), V::Int(b, sb)) => {
            if a == b {
                Ok(V::Int(*a, *sa))
            } else {
                let (w, sg) = if a > b { (*a, *sa) } else { (*b, *sb) };
                Ok(V::Int(w.max(64.min(w as u8).max(*a).max(*b)), sg))
            }
        }
        (V::Bool, V::Bool) => Ok(V::Bool),
        _ => bad(format!("operand types {:?} vs {:?}", l, r)),
    }
}
