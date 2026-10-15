//! The C subset's grammar: types, declarations, statements and expressions.
//!
//! A recursive-descent parser with precedence climbing. It builds a syntax
//! tree; name resolution, type checking and code generation all happen in
//! `compile`, so the tree here refers to variables and functions by name.
//!
//! The subset is deliberately small: one machine word (`int`), `char` (one
//! byte in memory, promoted to a word in expressions), pointers to either, and
//! `void`. No structs, typedefs, floating point, `switch`, `goto`, variadic
//! user functions, or preprocessor. That is enough for the programs this
//! compiler is meant to run -- arithmetic, strings, arrays, functions, loops --
//! and every omission is a coherent boundary rather than a hole.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::lex::{lex, Tok};

/// A type in the subset.
#[derive(Clone, Debug, PartialEq)]
pub enum Type {
    /// A machine word.
    Int,
    /// A byte.
    Char,
    /// No value.
    Void,
    /// A pointer.
    Ptr(Box<Type>),
}

impl Type {
    /// Bytes one element of this type occupies in memory.
    pub fn size(&self) -> u64 {
        match self {
            Type::Char => 1,
            _ => 8,
        }
    }

    /// What this type points at, if it is a pointer.
    pub fn pointee(&self) -> Type {
        match self {
            Type::Ptr(inner) => (**inner).clone(),
            _ => Type::Int,
        }
    }

    /// Whether values of this type are addresses.
    pub fn pointer(&self) -> bool {
        matches!(self, Type::Ptr(_))
    }
}

/// A declared variable: a global or a local.
#[derive(Clone, Debug)]
pub struct VarDecl {
    /// Name.
    pub name: String,
    /// Element type.
    pub ty: Type,
    /// Element count, when the declaration is an array.
    pub array: Option<usize>,
    /// Initialiser, if any.
    pub init: Option<Expr>,
}

/// A function.
#[derive(Clone, Debug)]
pub struct FuncDecl {
    /// Name.
    pub name: String,
    /// Return type.
    pub ret: Type,
    /// Parameters, in order.
    pub params: Vec<(String, Type)>,
    /// Body statements.
    pub body: Vec<Stmt>,
}

/// A whole translation unit.
#[derive(Clone, Debug)]
pub struct Program {
    /// File-scope variables.
    pub globals: Vec<VarDecl>,
    /// Functions.
    pub funcs: Vec<FuncDecl>,
}

/// A statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    /// One or more local declarations.
    Decl(Vec<VarDecl>),
    /// An expression evaluated for its side effects.
    Expr(Expr),
    /// `if`/`else`.
    If {
        /// Condition.
        cond: Expr,
        /// Taken when true.
        then: Vec<Stmt>,
        /// Taken when false (empty when there is no `else`).
        els: Vec<Stmt>,
    },
    /// `while`.
    While {
        /// Condition.
        cond: Expr,
        /// Body.
        body: Vec<Stmt>,
    },
    /// `for`.
    For {
        /// Initialiser.
        init: Option<Box<Stmt>>,
        /// Condition, or none for an infinite loop.
        cond: Option<Expr>,
        /// Step.
        step: Option<Expr>,
        /// Body.
        body: Vec<Stmt>,
    },
    /// `return`, with a value or not.
    Return(Option<Expr>),
    /// `break`.
    Break,
    /// `continue`.
    Continue,
    /// A bare block.
    Block(Vec<Stmt>),
}

/// An expression.
#[derive(Clone, Debug)]
pub enum Expr {
    /// An integer constant.
    Num(i64),
    /// A string literal, NUL-terminated.
    Str(Vec<u8>),
    /// A variable reference.
    Var(String),
    /// `target = value`.
    Assign {
        /// The lvalue.
        target: Box<Expr>,
        /// The value.
        value: Box<Expr>,
    },
    /// `target op= value`.
    Compound {
        /// The assignment operator (`+=`, ...).
        op: &'static str,
        /// The lvalue.
        target: Box<Expr>,
        /// The value.
        value: Box<Expr>,
    },
    /// A binary operator.
    Binary {
        /// The operator.
        op: &'static str,
        /// Left operand.
        lhs: Box<Expr>,
        /// Right operand.
        rhs: Box<Expr>,
    },
    /// A prefix unary operator.
    Unary {
        /// The operator.
        op: &'static str,
        /// The operand.
        expr: Box<Expr>,
    },
    /// `++`/`--`, prefix or postfix.
    IncDec {
        /// The lvalue.
        target: Box<Expr>,
        /// `+1` or `-1`.
        delta: i64,
        /// Whether the old value is the result.
        post: bool,
    },
    /// A function call.
    Call {
        /// Callee name.
        name: String,
        /// Arguments.
        args: Vec<Expr>,
    },
    /// `base[index]`.
    Index {
        /// Array or pointer.
        base: Box<Expr>,
        /// Index.
        index: Box<Expr>,
    },
    /// `cond ? then : els`.
    Ternary {
        /// Condition.
        cond: Box<Expr>,
        /// Value when true.
        then: Box<Expr>,
        /// Value when false.
        els: Box<Expr>,
    },
}

/// Parse a source file.
pub fn parse(src: &[u8]) -> Result<Program, String> {
    let tokens = lex(src)?;
    let mut parser = Parser { toks: tokens, pos: 0 };
    parser.program()
}

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

fn is_type_word(word: &str) -> bool {
    matches!(
        word,
        "int" | "char" | "void" | "long" | "short" | "unsigned" | "signed" | "const"
    )
}

impl Parser {
    fn peek(&self) -> &Tok {
        self.toks.get(self.pos).unwrap_or(&Tok::Eof)
    }

    fn peek_at(&self, ahead: usize) -> &Tok {
        self.toks.get(self.pos + ahead).unwrap_or(&Tok::Eof)
    }

    fn bump(&mut self) -> Tok {
        let token = self.peek().clone();
        self.pos += 1;
        token
    }

    fn eat(&mut self, punct: &str) -> bool {
        if matches!(self.peek(), Tok::Punct(found) if *found == punct) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, punct: &str) -> Result<(), String> {
        if self.eat(punct) {
            Ok(())
        } else {
            Err(format!("expected `{}`, found {:?}", punct, self.peek()))
        }
    }

    fn eat_word(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Tok::Ident(name) if name == word) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.bump() {
            Tok::Ident(name) => Ok(name),
            other => Err(format!("expected a name, found {:?}", other)),
        }
    }

    fn is_type_start(&self) -> bool {
        matches!(self.peek(), Tok::Ident(word) if is_type_word(word))
    }

    fn parse_type(&mut self) -> Result<Type, String> {
        let mut base: Option<Type> = None;
        loop {
            match self.peek() {
                Tok::Ident(word) if word == "const" || word == "unsigned" || word == "signed" || word == "long"
                    || word == "short" =>
                {
                    self.bump();
                }
                Tok::Ident(word) if word == "int" => {
                    self.bump();
                    base = Some(Type::Int);
                }
                Tok::Ident(word) if word == "char" => {
                    self.bump();
                    base = Some(Type::Char);
                }
                Tok::Ident(word) if word == "void" => {
                    self.bump();
                    base = Some(Type::Void);
                }
                _ => break,
            }
        }
        let mut ty = base.unwrap_or(Type::Int);
        while self.eat("*") {
            ty = Type::Ptr(Box::new(ty));
        }
        Ok(ty)
    }

    fn program(&mut self) -> Result<Program, String> {
        let mut globals = Vec::new();
        let mut funcs = Vec::new();
        while !matches!(self.peek(), Tok::Eof) {
            if self.eat(";") {
                continue;
            }
            let ty = self.parse_type()?;
            // A function is `type name (`; anything else is a variable list.
            if matches!(self.peek(), Tok::Ident(_)) && self.peek_at(1) == &Tok::Punct("(") {
                let name = self.ident()?;
                self.expect("(")?;
                let mut params = Vec::new();
                if !self.eat(")") {
                    if self.eat_word("void") && self.eat(")") {
                        // `f(void)` -- no parameters.
                    } else {
                        loop {
                            let pty = self.parse_type()?;
                            // A bare type in a parameter list (no name) is
                            // allowed and gets a placeholder.
                            let pname = if matches!(self.peek(), Tok::Ident(name) if !is_type_word(name)) {
                                self.ident()?
                            } else {
                                String::from("_")
                            };
                            if self.eat("[") {
                                let _ = self.expr()?;
                                self.expect("]")?;
                            }
                            params.push((pname, if pty == Type::Void { Type::Int } else { pty }));
                            if self.eat(",") {
                                continue;
                            }
                            self.expect(")")?;
                            break;
                        }
                    }
                }
                self.expect("{")?;
                let body = self.block()?;
                funcs.push(FuncDecl { name, ret: ty, params, body });
            } else {
                let mut decls = Vec::new();
                loop {
                    let name = self.ident()?;
                    let mut array = None;
                    if self.eat("[") {
                        let length = match self.bump() {
                            Tok::Num(n) if n >= 0 => n as usize,
                            other => return Err(format!("array length must be a constant, found {:?}", other)),
                        };
                        array = Some(length);
                        self.expect("]")?;
                    }
                    let init = if self.eat("=") { Some(self.assignment()?) } else { None };
                    decls.push(VarDecl { name, ty: ty.clone(), array, init });
                    if self.eat(",") {
                        continue;
                    }
                    break;
                }
                self.expect(";")?;
                globals.extend(decls);
            }
        }
        Ok(Program { globals, funcs })
    }

    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        let mut out = Vec::new();
        while !self.eat("}") {
            if matches!(self.peek(), Tok::Eof) {
                return Err(String::from("unexpected end of file inside a block"));
            }
            out.push(self.statement()?);
        }
        Ok(out)
    }

    /// A declaration statement, with the leading type already peeked.
    fn declaration(&mut self) -> Result<Stmt, String> {
        let ty = self.parse_type()?;
        let mut decls = Vec::new();
        loop {
            let name = self.ident()?;
            let mut array = None;
            if self.eat("[") {
                let length = match self.bump() {
                    Tok::Num(n) if n >= 0 => n as usize,
                    other => return Err(format!("array length must be a constant, found {:?}", other)),
                };
                array = Some(length);
                self.expect("]")?;
            }
            let init = if self.eat("=") { Some(self.assignment()?) } else { None };
            decls.push(VarDecl { name, ty: ty.clone(), array, init });
            if self.eat(",") {
                continue;
            }
            break;
        }
        self.expect(";")?;
        Ok(Stmt::Decl(decls))
    }

    fn statement(&mut self) -> Result<Stmt, String> {
        if self.eat(";") {
            return Ok(Stmt::Block(Vec::new()));
        }
        if self.eat("{") {
            return Ok(Stmt::Block(self.block()?));
        }
        if self.is_type_start() {
            return self.declaration();
        }
        if self.eat_word("if") {
            self.expect("(")?;
            let cond = self.expr()?;
            self.expect(")")?;
            let then = self.statement_or_block()?;
            let els = if self.eat_word("else") { self.statement_or_block()? } else { Vec::new() };
            return Ok(Stmt::If { cond, then, els });
        }
        if self.eat_word("while") {
            self.expect("(")?;
            let cond = self.expr()?;
            self.expect(")")?;
            let body = self.statement_or_block()?;
            return Ok(Stmt::While { cond, body });
        }
        if self.eat_word("for") {
            self.expect("(")?;
            let init = if self.eat(";") {
                None
            } else if self.is_type_start() {
                Some(Box::new(self.declaration()?))
            } else {
                let expr = self.expr()?;
                self.expect(";")?;
                Some(Box::new(Stmt::Expr(expr)))
            };
            let cond = if self.eat(";") {
                None
            } else {
                let expr = self.expr()?;
                self.expect(";")?;
                Some(expr)
            };
            let step = if self.eat(")") {
                None
            } else {
                let expr = self.expr()?;
                self.expect(")")?;
                Some(expr)
            };
            let body = self.statement_or_block()?;
            return Ok(Stmt::For { init, cond, step, body });
        }
        if self.eat_word("return") {
            if self.eat(";") {
                return Ok(Stmt::Return(None));
            }
            let expr = self.expr()?;
            self.expect(";")?;
            return Ok(Stmt::Return(Some(expr)));
        }
        if self.eat_word("break") {
            self.expect(";")?;
            return Ok(Stmt::Break);
        }
        if self.eat_word("continue") {
            self.expect(";")?;
            return Ok(Stmt::Continue);
        }
        let expr = self.expr()?;
        self.expect(";")?;
        Ok(Stmt::Expr(expr))
    }

    fn statement_or_block(&mut self) -> Result<Vec<Stmt>, String> {
        if self.eat("{") {
            self.block()
        } else {
            Ok(alloc::vec![self.statement()?])
        }
    }

    // -- Expressions --------------------------------------------------------

    fn expr(&mut self) -> Result<Expr, String> {
        self.assignment()
    }

    fn assignment(&mut self) -> Result<Expr, String> {
        let target = self.conditional()?;
        let op = match self.peek() {
            Tok::Punct(p) if matches!(*p, "=" | "+=" | "-=" | "*=" | "/=" | "%=" | "&=" | "|=" | "^=" | "<<=" | ">>=") => *p,
            _ => return Ok(target),
        };
        self.bump();
        let value = self.assignment()?;
        if op == "=" {
            Ok(Expr::Assign { target: Box::new(target), value: Box::new(value) })
        } else {
            Ok(Expr::Compound { op, target: Box::new(target), value: Box::new(value) })
        }
    }

    fn conditional(&mut self) -> Result<Expr, String> {
        let cond = self.logical_or()?;
        if self.eat("?") {
            let then = self.expr()?;
            self.expect(":")?;
            let els = self.conditional()?;
            Ok(Expr::Ternary { cond: Box::new(cond), then: Box::new(then), els: Box::new(els) })
        } else {
            Ok(cond)
        }
    }

    fn binary(&mut self, ops: &[&'static str], next: fn(&mut Self) -> Result<Expr, String>) -> Result<Expr, String> {
        let mut lhs = next(self)?;
        loop {
            let op = match self.peek() {
                Tok::Punct(p) if ops.contains(p) => *p,
                _ => break,
            };
            self.bump();
            let rhs = next(self)?;
            lhs = Expr::Binary { op, lhs: Box::new(lhs), rhs: Box::new(rhs) };
        }
        Ok(lhs)
    }

    fn logical_or(&mut self) -> Result<Expr, String> {
        self.binary(&["||"], Self::logical_and)
    }
    fn logical_and(&mut self) -> Result<Expr, String> {
        self.binary(&["&&"], Self::bit_or)
    }
    fn bit_or(&mut self) -> Result<Expr, String> {
        self.binary(&["|"], Self::bit_xor)
    }
    fn bit_xor(&mut self) -> Result<Expr, String> {
        self.binary(&["^"], Self::bit_and)
    }
    fn bit_and(&mut self) -> Result<Expr, String> {
        self.binary(&["&"], Self::equality)
    }
    fn equality(&mut self) -> Result<Expr, String> {
        self.binary(&["==", "!="], Self::relational)
    }
    fn relational(&mut self) -> Result<Expr, String> {
        self.binary(&["<", "<=", ">", ">="], Self::shift)
    }
    fn shift(&mut self) -> Result<Expr, String> {
        self.binary(&["<<", ">>"], Self::additive)
    }
    fn additive(&mut self) -> Result<Expr, String> {
        self.binary(&["+", "-"], Self::multiplicative)
    }
    fn multiplicative(&mut self) -> Result<Expr, String> {
        self.binary(&["*", "/", "%"], Self::unary)
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if let Tok::Punct(p) = self.peek() {
            match *p {
                "-" | "+" | "!" | "~" | "&" | "*" => {
                    let op = *p;
                    self.bump();
                    let expr = self.unary()?;
                    return Ok(Expr::Unary { op, expr: Box::new(expr) });
                }
                "++" | "--" => {
                    let delta = if *p == "++" { 1 } else { -1 };
                    self.bump();
                    let target = self.unary()?;
                    return Ok(Expr::IncDec { target: Box::new(target), delta, post: false });
                }
                _ => {}
            }
        }
        self.postfix()
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut expr = self.primary()?;
        loop {
            match self.peek() {
                Tok::Punct("[") => {
                    self.bump();
                    let index = self.expr()?;
                    self.expect("]")?;
                    expr = Expr::Index { base: Box::new(expr), index: Box::new(index) };
                }
                Tok::Punct("(") => {
                    self.bump();
                    let mut args = Vec::new();
                    if !self.eat(")") {
                        loop {
                            args.push(self.assignment()?);
                            if self.eat(",") {
                                continue;
                            }
                            self.expect(")")?;
                            break;
                        }
                    }
                    let name = match expr {
                        Expr::Var(name) => name,
                        _ => return Err(String::from("call of something that is not a function")),
                    };
                    expr = Expr::Call { name, args };
                }
                Tok::Punct("++") => {
                    self.bump();
                    expr = Expr::IncDec { target: Box::new(expr), delta: 1, post: true };
                }
                Tok::Punct("--") => {
                    self.bump();
                    expr = Expr::IncDec { target: Box::new(expr), delta: -1, post: true };
                }
                Tok::Punct(".") | Tok::Punct("->") => {
                    return Err(String::from("struct members are not part of this subset"));
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.bump() {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(bytes) => Ok(Expr::Str(bytes)),
            Tok::Ident(name) if name == "sizeof" => Err(String::from("sizeof is not implemented")),
            Tok::Ident(name) => Ok(Expr::Var(name)),
            Tok::Punct("(") => {
                let expr = self.expr()?;
                self.expect(")")?;
                Ok(expr)
            }
            other => Err(format!("unexpected token {:?}", other)),
        }
    }
}
