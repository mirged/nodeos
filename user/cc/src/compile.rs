//! Code generation: syntax tree in, [`Program`] out.
//!
//! Name resolution (locals versus globals), the small amount of type checking
//! the subset needs, and the lowering of statements and expressions to stack
//! machine instructions all live here. Types are checked as code is emitted,
//! which is why there is no separate pass: an `Index` has to know its element
//! size to scale the subscript, and that size comes from the base expression's
//! type.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::parse::{Expr, FuncDecl, Program as Ast, Stmt, Type, VarDecl};
use crate::vm::{builtin, FuncInfo, Ins, Program, DATA_BASE};

/// A local variable: a frame offset and its declared type.
#[derive(Clone)]
struct LocalSym {
    name: String,
    off: u32,
    ty: Type,
    array: Option<usize>,
}

/// What a name resolves to.
#[derive(Clone)]
enum Sym {
    Local(LocalSym),
    Global { addr: u64, ty: Type, array: Option<usize> },
}

/// A loop, for resolving `break` and `continue` while emitting it.
struct Loop {
    breaks: Vec<usize>,
    continues: Vec<usize>,
}

struct Compiler {
    code: Vec<Ins>,
    data: Vec<u8>,
    strings: Vec<(Vec<u8>, u64)>,
    globals: Vec<(String, u64, Type, Option<usize>)>,
    heap_base: u64,
    funcs: Vec<FuncInfo>,
    func_names: Vec<String>,
    func_rets: Vec<Type>,
    scopes: Vec<Vec<LocalSym>>,
    frame: u32,
    temp: u32,
    loops: Vec<Loop>,
}

/// Compile a parsed program.
pub fn compile(ast: &Ast) -> Result<Program, String> {
    let mut compiler = Compiler::new();
    compiler.collect_strings(ast);
    compiler.assign_globals(ast);
    compiler.assign_functions(ast);

    // `__start`: run the global initialisers, call `main`, stop.
    compiler.scopes.clear();
    compiler.scopes.push(Vec::new());
    compiler.frame = 0;
    compiler.temp = 0;
    let entry = compiler.code.len();
    for global in &ast.globals {
        if global.array.is_none() {
            if let Some(init) = &global.init {
                let address = compiler.global_addr(&global.name)?;
                compiler.emit(Ins::Push(address as i64));
                compiler.rvalue(init)?;
                compiler.emit(Ins::Dup);
                compiler.store(global.ty.clone());
                compiler.emit(Ins::Pop);
            }
        }
    }
    let main = compiler.func_index("main").ok_or_else(|| String::from("no `main` function is defined"))?;
    compiler.emit(Ins::Call(main, 0));
    compiler.emit(Ins::Halt);

    for (index, function) in ast.funcs.iter().enumerate() {
        let (function_entry, frame) = compiler.emit_function(function)?;
        compiler.funcs[index].entry = function_entry;
        compiler.funcs[index].frame = frame;
    }

    Ok(Program {
        code: compiler.code,
        funcs: compiler.funcs,
        data: compiler.data,
        heap_base: compiler.heap_base,
        entry,
    })
}

impl Compiler {
    fn new() -> Self {
        Self {
            code: Vec::new(),
            data: Vec::new(),
            strings: Vec::new(),
            globals: Vec::new(),
            heap_base: DATA_BASE,
            funcs: Vec::new(),
            func_names: Vec::new(),
            func_rets: Vec::new(),
            scopes: Vec::new(),
            frame: 0,
            temp: 0,
            loops: Vec::new(),
        }
    }

    fn emit(&mut self, ins: Ins) {
        self.code.push(ins);
    }

    fn emit_open(&mut self, ins: Ins) -> usize {
        let at = self.code.len();
        self.code.push(ins);
        at
    }

    fn patch(&mut self, at: usize, target: usize) {
        self.code[at] = match self.code[at].clone() {
            Ins::Jump(_) => Ins::Jump(target),
            Ins::JumpZ(_) => Ins::JumpZ(target),
            Ins::JumpNZ(_) => Ins::JumpNZ(target),
            other => other,
        };
    }

    fn load(&mut self, ty: Type) {
        if matches!(ty, Type::Char) {
            self.emit(Ins::LoadChar);
        } else {
            self.emit(Ins::LoadMem);
        }
    }

    fn store(&mut self, ty: Type) {
        if matches!(ty, Type::Char) {
            self.emit(Ins::StoreChar);
        } else {
            self.emit(Ins::StoreMem);
        }
    }

    /// Store the word at the address below it, leaving the value on the stack.
    fn store_keep(&mut self, ty: Type) {
        if matches!(ty, Type::Char) {
            self.emit(Ins::StoreCharKeep);
        } else {
            self.emit(Ins::StoreMemKeep);
        }
    }

    // -- Data ---------------------------------------------------------------

    fn add_string(&mut self, bytes: &[u8]) -> u64 {
        if let Some((_, address)) = self.strings.iter().find(|(existing, _)| existing.as_slice() == bytes) {
            return *address;
        }
        let address = DATA_BASE + self.data.len() as u64;
        self.data.extend_from_slice(bytes);
        while self.data.len() % 8 != 0 {
            self.data.push(0);
        }
        self.strings.push((bytes.to_vec(), address));
        address
    }

    fn assign_globals(&mut self, ast: &Ast) {
        for global in &ast.globals {
            let size = match global.array {
                Some(len) => (len as u64).max(1) * global.ty.size(),
                None => 8,
            };
            while self.data.len() % 8 != 0 {
                self.data.push(0);
            }
            let address = DATA_BASE + self.data.len() as u64;
            for _ in 0..size {
                self.data.push(0);
            }
            self.globals
                .push((global.name.clone(), address, global.ty.clone(), global.array));
        }
        let mut heap = DATA_BASE + self.data.len() as u64;
        while heap % 16 != 0 {
            heap += 1;
        }
        self.heap_base = heap;
    }

    fn assign_functions(&mut self, ast: &Ast) {
        for function in &ast.funcs {
            self.func_names.push(function.name.clone());
            self.func_rets.push(function.ret.clone());
            self.funcs.push(FuncInfo { entry: 0, frame: 0 });
        }
    }

    fn collect_strings(&mut self, ast: &Ast) {
        for global in &ast.globals {
            if let Some(init) = &global.init {
                self.walk(init);
            }
        }
        for function in &ast.funcs {
            for statement in &function.body {
                self.walk_stmt(statement);
            }
        }
    }

    fn walk_stmt(&mut self, statement: &Stmt) {
        match statement {
            Stmt::Decl(decls) => {
                for decl in decls {
                    if let Some(init) = &decl.init {
                        self.walk(init);
                    }
                }
            }
            Stmt::Expr(expr) => self.walk(expr),
            Stmt::If { cond, then, els } => {
                self.walk(cond);
                for statement in then {
                    self.walk_stmt(statement);
                }
                for statement in els {
                    self.walk_stmt(statement);
                }
            }
            Stmt::While { cond, body } => {
                self.walk(cond);
                for statement in body {
                    self.walk_stmt(statement);
                }
            }
            Stmt::For { init, cond, step, body } => {
                if let Some(init) = init {
                    self.walk_stmt(init);
                }
                if let Some(cond) = cond {
                    self.walk(cond);
                }
                if let Some(step) = step {
                    self.walk(step);
                }
                for statement in body {
                    self.walk_stmt(statement);
                }
            }
            Stmt::Return(expr) => {
                if let Some(expr) = expr {
                    self.walk(expr);
                }
            }
            Stmt::Block(body) => {
                for statement in body {
                    self.walk_stmt(statement);
                }
            }
            Stmt::Break | Stmt::Continue => {}
        }
    }

    fn walk(&mut self, expr: &Expr) {
        match expr {
            Expr::Num(_) | Expr::Var(_) => {}
            Expr::Str(bytes) => {
                self.add_string(bytes);
            }
            Expr::Assign { target, value } | Expr::Compound { target, value, .. } => {
                self.walk(target);
                self.walk(value);
            }
            Expr::Binary { lhs, rhs, .. } => {
                self.walk(lhs);
                self.walk(rhs);
            }
            Expr::Unary { expr, .. } => self.walk(expr),
            Expr::IncDec { target, .. } => self.walk(target),
            Expr::Call { args, .. } => {
                for arg in args {
                    self.walk(arg);
                }
            }
            Expr::Index { base, index } => {
                self.walk(base);
                self.walk(index);
            }
            Expr::Ternary { cond, then, els } => {
                self.walk(cond);
                self.walk(then);
                self.walk(els);
            }
        }
    }

    // -- Names --------------------------------------------------------------

    fn lookup(&self, name: &str) -> Result<Sym, String> {
        for scope in self.scopes.iter().rev() {
            if let Some(local) = scope.iter().rev().find(|local| local.name == name) {
                return Ok(Sym::Local(local.clone()));
            }
        }
        if let Some((_, address, ty, array)) = self.globals.iter().find(|global| global.0 == name) {
            return Ok(Sym::Global { addr: *address, ty: ty.clone(), array: *array });
        }
        Err(format!("undefined variable `{name}`"))
    }

    fn global_addr(&self, name: &str) -> Result<u64, String> {
        self.globals
            .iter()
            .find(|global| global.0 == name)
            .map(|global| global.1)
            .ok_or_else(|| format!("undefined global `{name}`"))
    }

    fn func_index(&self, name: &str) -> Option<usize> {
        self.func_names.iter().position(|candidate| candidate == name)
    }

    // -- Statements ---------------------------------------------------------

    fn stmts(&mut self, list: &[Stmt]) -> Result<(), String> {
        self.scopes.push(Vec::new());
        for statement in list {
            self.stmt(statement)?;
        }
        self.scopes.pop();
        Ok(())
    }

    fn stmt(&mut self, statement: &Stmt) -> Result<(), String> {
        match statement {
            Stmt::Decl(decls) => {
                for decl in decls {
                    self.declare(decl)?;
                }
            }
            Stmt::Expr(expr) => {
                self.rvalue(expr)?;
                self.emit(Ins::Pop);
            }
            Stmt::Block(body) => self.stmts(body)?,
            Stmt::If { cond, then, els } => {
                self.rvalue(cond)?;
                let to_else = self.emit_open(Ins::JumpZ(0));
                self.stmts(then)?;
                if els.is_empty() {
                    let end = self.code.len();
                    self.patch(to_else, end);
                } else {
                    let to_end = self.emit_open(Ins::Jump(0));
                    let here = self.code.len();
                    self.patch(to_else, here);
                    self.stmts(els)?;
                    let end = self.code.len();
                    self.patch(to_end, end);
                }
            }
            Stmt::While { cond, body } => {
                let top = self.code.len();
                self.rvalue(cond)?;
                let to_end = self.emit_open(Ins::JumpZ(0));
                self.loops.push(Loop { breaks: Vec::new(), continues: Vec::new() });
                self.stmts(body)?;
                self.close_loop(top);
                self.emit(Ins::Jump(top));
                let end = self.code.len();
                self.patch(to_end, end);
            }
            Stmt::For { init, cond, step, body } => {
                self.scopes.push(Vec::new());
                if let Some(init) = init {
                    self.stmt(init)?;
                }
                let top = self.code.len();
                let to_end = if let Some(cond) = cond {
                    self.rvalue(cond)?;
                    Some(self.emit_open(Ins::JumpZ(0)))
                } else {
                    None
                };
                self.loops.push(Loop { breaks: Vec::new(), continues: Vec::new() });
                self.stmts(body)?;
                let step_target = self.code.len();
                self.close_loop(step_target);
                if let Some(step) = step {
                    self.rvalue(step)?;
                    self.emit(Ins::Pop);
                }
                self.emit(Ins::Jump(top));
                let end = self.code.len();
                if let Some(to_end) = to_end {
                    self.patch(to_end, end);
                }
                self.scopes.pop();
            }
            Stmt::Return(value) => {
                match value {
                    Some(expr) => {
                        self.rvalue(expr)?;
                    }
                    None => self.emit(Ins::Push(0)),
                }
                self.emit(Ins::Ret);
            }
            Stmt::Break => {
                if !self.loops.is_empty() {
                    let at = self.emit_open(Ins::Jump(0));
                    self.loops.last_mut().unwrap().breaks.push(at);
                }
            }
            Stmt::Continue => {
                if !self.loops.is_empty() {
                    let at = self.emit_open(Ins::Jump(0));
                    self.loops.last_mut().unwrap().continues.push(at);
                }
            }
        }
        Ok(())
    }

    fn close_loop(&mut self, continue_target: usize) {
        if let Some(loop_) = self.loops.pop() {
            for at in &loop_.continues {
                self.patch(*at, continue_target);
            }
            let end = self.code.len();
            for at in &loop_.breaks {
                self.patch(*at, end);
            }
        }
    }

    fn declare(&mut self, decl: &VarDecl) -> Result<(), String> {
        let size = match decl.array {
            Some(len) => (len as u64).max(1) * decl.ty.size(),
            None => 8,
        };
        self.frame = ((self.frame as u64 + 7) & !7) as u32;
        let off = self.frame;
        self.frame += size as u32;
        let scope = self.scopes.last_mut().ok_or_else(|| String::from("internal: no scope"))?;
        scope.push(LocalSym { name: decl.name.clone(), off, ty: decl.ty.clone(), array: decl.array });
        if decl.array.is_none() {
            if let Some(init) = &decl.init {
                self.rvalue(init)?;
                self.emit(Ins::StoreLocal(off));
            }
        }
        Ok(())
    }

    fn emit_function(&mut self, function: &FuncDecl) -> Result<(usize, u32), String> {
        let entry = self.code.len();
        self.scopes.clear();
        self.scopes.push(Vec::new());
        self.frame = 0;
        self.loops.clear();
        for (index, (name, ty)) in function.params.iter().enumerate() {
            let off = (index as u32) * 8;
            self.scopes
                .last_mut()
                .unwrap()
                .push(LocalSym { name: name.clone(), off, ty: ty.clone(), array: None });
            self.frame = off + 8;
        }
        self.temp = self.frame;
        self.frame += 8;
        self.stmts(&function.body)?;
        self.emit(Ins::Push(0));
        self.emit(Ins::Ret);
        Ok((entry, self.frame))
    }

    // -- Expressions --------------------------------------------------------

    fn lvalue(&mut self, expr: &Expr) -> Result<Type, String> {
        match expr {
            Expr::Var(name) => match self.lookup(name)? {
                Sym::Local(local) => {
                    if local.array.is_some() {
                        return Err(format!("`{name}` is an array and cannot be assigned"));
                    }
                    self.emit(Ins::LocalAddr(local.off));
                    Ok(local.ty)
                }
                Sym::Global { addr, ty, array } => {
                    if array.is_some() {
                        return Err(format!("`{name}` is an array and cannot be assigned"));
                    }
                    self.emit(Ins::Push(addr as i64));
                    Ok(ty)
                }
            },
            Expr::Unary { op: "*", expr } => {
                let ty = self.rvalue(expr)?;
                if !ty.pointer() {
                    return Err(String::from("dereference of a non-pointer"));
                }
                Ok(ty.pointee())
            }
            Expr::Index { base, index } => self.index_addr(base, index),
            _ => Err(String::from("expression is not assignable")),
        }
    }

    fn index_addr(&mut self, base: &Expr, index: &Expr) -> Result<Type, String> {
        let base_ty = self.rvalue(base)?;
        if !base_ty.pointer() {
            return Err(String::from("subscript of something that is not an array or pointer"));
        }
        let elem = base_ty.pointee();
        let size = elem.size();
        self.rvalue(index)?;
        if size != 1 {
            self.emit(Ins::Push(size as i64));
            self.emit(Ins::Mul);
        }
        self.emit(Ins::Add);
        Ok(elem)
    }

    fn rvalue(&mut self, expr: &Expr) -> Result<Type, String> {
        match expr {
            Expr::Num(value) => {
                self.emit(Ins::Push(*value));
                Ok(Type::Int)
            }
            Expr::Str(bytes) => {
                let address = self.add_string(bytes);
                self.emit(Ins::Push(address as i64));
                Ok(Type::Ptr(Box::new(Type::Char)))
            }
            Expr::Var(name) => match self.lookup(name)? {
                Sym::Local(local) => {
                    if local.array.is_some() {
                        self.emit(Ins::LocalAddr(local.off));
                        Ok(Type::Ptr(Box::new(local.ty)))
                    } else {
                        self.emit(Ins::LoadLocal(local.off));
                        Ok(local.ty)
                    }
                }
                Sym::Global { addr, ty, array } => {
                    if array.is_some() {
                        self.emit(Ins::Push(addr as i64));
                        Ok(Type::Ptr(Box::new(ty)))
                    } else {
                        self.emit(Ins::LoadGlobal(addr));
                        Ok(ty)
                    }
                }
            },
            Expr::Assign { target, value } => {
                let cell = self.lvalue(target)?;
                self.rvalue(value)?;
                self.store_keep(cell.clone());
                Ok(cell)
            }
            Expr::Compound { op, target, value } => {
                let cell = self.lvalue(target)?;
                self.emit(Ins::Dup);
                self.load(cell.clone());
                self.rvalue(value)?;
                let base = &op[..op.len() - 1];
                let ins = bin_ins(base).ok_or_else(|| format!("unsupported operator `{op}`"))?;
                self.emit(ins);
                self.store_keep(cell.clone());
                Ok(cell)
            }
            Expr::Binary { op, lhs, rhs } => self.binary(op, lhs, rhs),
            Expr::Unary { op, expr } => self.unary(op, expr),
            Expr::IncDec { target, delta, post } => self.incdec(target, *delta, *post),
            Expr::Call { name, args } => self.call(name, args),
            Expr::Index { base, index } => {
                let elem = self.index_addr(base, index)?;
                self.load(elem.clone());
                Ok(elem)
            }
            Expr::Ternary { cond, then, els } => {
                self.rvalue(cond)?;
                let to_else = self.emit_open(Ins::JumpZ(0));
                let then_ty = self.rvalue(then)?;
                let to_end = self.emit_open(Ins::Jump(0));
                let here = self.code.len();
                self.patch(to_else, here);
                let else_ty = self.rvalue(els)?;
                let end = self.code.len();
                self.patch(to_end, end);
                Ok(if matches!(then_ty, Type::Void) { else_ty } else { then_ty })
            }
        }
    }

    fn binary(&mut self, op: &str, lhs: &Expr, rhs: &Expr) -> Result<Type, String> {
        if op == "&&" {
            self.rvalue(lhs)?;
            let first = self.emit_open(Ins::JumpZ(0));
            self.rvalue(rhs)?;
            let second = self.emit_open(Ins::JumpZ(0));
            self.emit(Ins::Push(1));
            let to_end = self.emit_open(Ins::Jump(0));
            let zero = self.code.len();
            self.patch(first, zero);
            self.patch(second, zero);
            self.emit(Ins::Push(0));
            let end = self.code.len();
            self.patch(to_end, end);
            return Ok(Type::Int);
        }
        if op == "||" {
            self.rvalue(lhs)?;
            let first = self.emit_open(Ins::JumpNZ(0));
            self.rvalue(rhs)?;
            let second = self.emit_open(Ins::JumpNZ(0));
            self.emit(Ins::Push(0));
            let to_end = self.emit_open(Ins::Jump(0));
            let one = self.code.len();
            self.patch(first, one);
            self.patch(second, one);
            self.emit(Ins::Push(1));
            let end = self.code.len();
            self.patch(to_end, end);
            return Ok(Type::Int);
        }
        if let Some(ins) = compare_ins(op) {
            self.rvalue(lhs)?;
            self.rvalue(rhs)?;
            self.emit(ins);
            return Ok(Type::Int);
        }

        let left = self.rvalue(lhs)?;
        let right = self.rvalue(rhs)?;
        let mut result = Type::Int;
        if matches!(op, "+" | "-") {
            if left.pointer() {
                let size = left.pointee().size();
                if size != 1 {
                    self.emit(Ins::Push(size as i64));
                    self.emit(Ins::Mul);
                }
                result = left;
            } else if right.pointer() && op == "+" {
                // `int + pointer`: scale the integer side, which is on the
                // bottom of the stack.
                self.emit(Ins::Swap);
                let size = right.pointee().size();
                if size != 1 {
                    self.emit(Ins::Push(size as i64));
                    self.emit(Ins::Mul);
                }
                result = right;
            }
        }
        let ins = bin_ins(op).ok_or_else(|| format!("unsupported operator `{op}`"))?;
        self.emit(ins);
        Ok(result)
    }

    fn unary(&mut self, op: &str, expr: &Expr) -> Result<Type, String> {
        match op {
            "-" => {
                self.rvalue(expr)?;
                self.emit(Ins::Neg);
                Ok(Type::Int)
            }
            "+" => self.rvalue(expr),
            "!" => {
                self.rvalue(expr)?;
                self.emit(Ins::Push(0));
                self.emit(Ins::Eq);
                Ok(Type::Int)
            }
            "~" => {
                self.rvalue(expr)?;
                self.emit(Ins::Not);
                Ok(Type::Int)
            }
            "&" => {
                let cell = self.lvalue(expr)?;
                Ok(Type::Ptr(Box::new(cell)))
            }
            "*" => {
                let ty = self.rvalue(expr)?;
                if !ty.pointer() {
                    return Err(String::from("dereference of a non-pointer"));
                }
                let elem = ty.pointee();
                self.load(elem.clone());
                Ok(elem)
            }
            _ => Err(format!("unsupported unary operator `{op}`")),
        }
    }

    fn incdec(&mut self, target: &Expr, delta: i64, post: bool) -> Result<Type, String> {
        let cell = self.lvalue(target)?;
        self.emit(Ins::Dup);
        self.load(cell.clone());
        if post {
            // Save the old value, compute and store the new one, then put the
            // old value back on top: `x++` evaluates to what `x` was.
            self.emit(Ins::StoreLocal(self.temp));
            self.emit(Ins::LoadLocal(self.temp));
            self.emit(Ins::Push(delta));
            self.emit(Ins::Add);
            self.store_keep(cell.clone());
            self.emit(Ins::LoadLocal(self.temp));
            self.emit(Ins::Swap);
            self.emit(Ins::Pop);
        } else {
            self.emit(Ins::Push(delta));
            self.emit(Ins::Add);
            self.store_keep(cell.clone());
        }
        Ok(cell)
    }

    fn call(&mut self, name: &str, args: &[Expr]) -> Result<Type, String> {
        if let Some((id, ret)) = builtin_of(name) {
            for arg in args {
                self.rvalue(arg)?;
            }
            self.emit(Ins::Builtin(id, args.len() as u32));
            return Ok(ret);
        }
        let index = self.func_index(name).ok_or_else(|| format!("undefined function `{name}`"))?;
        let ret = self.func_rets.get(index).cloned().unwrap_or(Type::Int);
        for arg in args {
            self.rvalue(arg)?;
        }
        self.emit(Ins::Call(index, args.len() as u32));
        Ok(ret)
    }
}

fn bin_ins(op: &str) -> Option<Ins> {
    Some(match op {
        "+" => Ins::Add,
        "-" => Ins::Sub,
        "*" => Ins::Mul,
        "/" => Ins::Div,
        "%" => Ins::Mod,
        "&" => Ins::And,
        "|" => Ins::Or,
        "^" => Ins::Xor,
        "<<" => Ins::Shl,
        ">>" => Ins::Shr,
        _ => return None,
    })
}

fn compare_ins(op: &str) -> Option<Ins> {
    Some(match op {
        "==" => Ins::Eq,
        "!=" => Ins::Ne,
        "<" => Ins::Lt,
        "<=" => Ins::Le,
        ">" => Ins::Gt,
        ">=" => Ins::Ge,
        _ => return None,
    })
}

/// The builtins, by name, with their return types.
fn builtin_of(name: &str) -> Option<(u32, Type)> {
    let int = Type::Int;
    let void = Type::Void;
    let string = Type::Ptr(Box::new(Type::Char));
    Some(match name {
        "putchar" => (builtin::PUTCHAR, int),
        "print" => (builtin::PRINT, void),
        "puts" => (builtin::PUTS, void),
        "printf" => (builtin::PRINTF, int),
        "strlen" => (builtin::STRLEN, int),
        "strcmp" => (builtin::STRCMP, int),
        "strcpy" => (builtin::STRCPY, string),
        "strcat" => (builtin::STRCAT, string),
        "atoi" => (builtin::ATOI, int),
        "abs" => (builtin::ABS, int),
        "min" => (builtin::MIN, int),
        "max" => (builtin::MAX, int),
        "malloc" => (builtin::MALLOC, string),
        "free" => (builtin::FREE, void),
        "memset" => (builtin::MEMSET, void),
        "memcpy" => (builtin::MEMCPY, void),
        "rand" => (builtin::RAND, int),
        "srand" => (builtin::SRAND, void),
        "exit" => (builtin::EXIT, void),
        "print_int" => (builtin::PRINT_INT, void),
        _ => return None,
    })
}
