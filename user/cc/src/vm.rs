//! The virtual machine the compiler targets.
//!
//! A stack machine over a flat, byte-addressable memory. Every value is a
//! 64-bit word; `char` lives in a single byte in memory and is widened when it
//! is loaded. Globals and string data sit at the bottom of memory, a bump heap
//! grows up from there, and call frames grow down from the top -- so `&local`
//! and `malloc` hand out addresses from the same space, which is what makes
//! pointers uniform.
//!
//! The "bytecode" is a `Vec<Ins>`, not encoded bytes: this is a teaching
//! compiler, and a jump target as a `usize` index is far easier to get right
//! than a byte offset.

use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;

/// Total address space, in bytes. Globals at the bottom, frames at the top,
/// heap in between; the compiler's own heap must be larger than this.
pub const MEM_SIZE: usize = 8 * 1024 * 1024;
/// Where the data segment starts; address 0 is left as NULL.
pub const DATA_BASE: u64 = 0x1000;

/// One instruction.
#[derive(Clone, Debug)]
pub enum Ins {
    /// Push a constant.
    Push(i64),
    /// Push the 8-byte word at `fp + offset`.
    LoadLocal(u32),
    /// Pop a word and store it at `fp + offset`.
    StoreLocal(u32),
    /// Push the address `fp + offset`.
    LocalAddr(u32),
    /// Push the 8-byte word at a fixed address.
    LoadGlobal(u64),
    /// Pop an address, push the 8-byte word there.
    LoadMem,
    /// Pop a value, then an address, and store the word there.
    StoreMem,
    /// As [`Ins::StoreMem`], but push the value back: an assignment is an
    /// expression, so `x = y` has to leave `y` behind.
    StoreMemKeep,
    /// Pop an address, push the zero-extended byte there.
    LoadChar,
    /// Pop a value, then an address, and store its low byte there.
    StoreChar,
    /// As [`Ins::StoreChar`], but push the value back.
    StoreCharKeep,
    /// Exchange the top two stack words.
    Swap,
    /// Discard the top word.
    Pop,
    /// Duplicate the top word.
    Dup,
    /// Arithmetic.
    Add,
    /// See [`Ins::Add`].
    Sub,
    /// See [`Ins::Add`].
    Mul,
    /// See [`Ins::Add`].
    Div,
    /// See [`Ins::Add`].
    Mod,
    /// Arithmetic negation.
    Neg,
    /// Bitwise AND.
    And,
    /// Bitwise OR.
    Or,
    /// Bitwise XOR.
    Xor,
    /// Bitwise NOT.
    Not,
    /// Shift left.
    Shl,
    /// Arithmetic shift right.
    Shr,
    /// Push 1 when equal, else 0.
    Eq,
    /// Push 1 when not equal, else 0.
    Ne,
    /// Push 1 when less than, else 0.
    Lt,
    /// Push 1 when less or equal, else 0.
    Le,
    /// Push 1 when greater, else 0.
    Gt,
    /// Push 1 when greater or equal, else 0.
    Ge,
    /// Unconditional jump.
    Jump(usize),
    /// Pop, and jump when it is zero.
    JumpZ(usize),
    /// Pop, and jump when it is not zero.
    JumpNZ(usize),
    /// Call a user function with `argc` arguments on the stack.
    Call(usize, u32),
    /// Call a standard-library builtin with `argc` arguments on the stack.
    Builtin(u32, u32),
    /// Return from a function, popping the return value.
    Ret,
    /// Stop the machine; the top of the stack is the exit status.
    Halt,
}

/// Where a function lives and how much frame it needs.
#[derive(Clone, Debug)]
pub struct FuncInfo {
    /// Index of its first instruction.
    pub entry: usize,
    /// Bytes of frame (parameters, locals and the scratch slot).
    pub frame: u32,
}

/// A compiled program.
pub struct Program {
    /// Instructions.
    pub code: Vec<Ins>,
    /// Functions, by index.
    pub funcs: Vec<FuncInfo>,
    /// Initialised data (strings and globals), placed at [`DATA_BASE`].
    pub data: Vec<u8>,
    /// First heap byte; the compiler sets it past the data.
    pub heap_base: u64,
    /// The `__start` code the machine begins at.
    pub entry: usize,
}

/// Standard-library entry points, resolved by the compiler by name.
pub mod builtin {
    /// Write one byte.
    pub const PUTCHAR: u32 = 1;
    /// Write a NUL-terminated string.
    pub const PRINT: u32 = 2;
    /// Write a string and a newline.
    pub const PUTS: u32 = 3;
    /// Formatted output.
    pub const PRINTF: u32 = 4;
    /// Length of a NUL-terminated string.
    pub const STRLEN: u32 = 5;
    /// Compare two strings.
    pub const STRCMP: u32 = 6;
    /// Copy a string.
    pub const STRCPY: u32 = 7;
    /// Append a string.
    pub const STRCAT: u32 = 8;
    /// Parse a decimal integer.
    pub const ATOI: u32 = 9;
    /// Absolute value.
    pub const ABS: u32 = 10;
    /// Smaller of two values.
    pub const MIN: u32 = 11;
    /// Larger of two values.
    pub const MAX: u32 = 12;
    /// Allocate from the bump heap.
    pub const MALLOC: u32 = 13;
    /// Release (a no-op in this runtime).
    pub const FREE: u32 = 14;
    /// Fill memory.
    pub const MEMSET: u32 = 15;
    /// Copy memory.
    pub const MEMCPY: u32 = 16;
    /// Next pseudo-random number.
    pub const RAND: u32 = 17;
    /// Seed the generator.
    pub const SRAND: u32 = 18;
    /// Stop the program.
    pub const EXIT: u32 = 19;
    /// Write a decimal integer.
    pub const PRINT_INT: u32 = 20;
}

struct Machine<'a> {
    program: &'a Program,
    mem: Vec<u8>,
    stack: Vec<i64>,
    call: Vec<(usize, i64)>,
    fp: i64,
    pc: usize,
    heap: usize,
    output: Vec<u8>,
    rng: u64,
    exit: Option<i64>,
}

/// Run a compiled program, returning its output and exit status.
pub fn run(program: &Program) -> (String, i64) {
    let mut mem = vec![0u8; MEM_SIZE];
    let end = (DATA_BASE as usize + program.data.len()).min(MEM_SIZE);
    mem[DATA_BASE as usize..end].copy_from_slice(&program.data[..end - DATA_BASE as usize]);

    let mut machine = Machine {
        program,
        mem,
        stack: Vec::new(),
        call: Vec::new(),
        fp: MEM_SIZE as i64,
        pc: program.entry,
        heap: program.heap_base as usize,
        output: Vec::new(),
        rng: 0x2545_F491_4F6C_DD1D,
        exit: None,
    };
    machine.execute();
    (String::from_utf8_lossy(&machine.output).into_owned(), machine.exit.unwrap_or(0))
}

impl<'a> Machine<'a> {
    fn execute(&mut self) {
        // A generous cap on steps stops a runaway loop from hanging the only
        // process the shell is waiting on.
        let mut budget = 200_000_000u64;
        while self.pc < self.program.code.len() {
            if budget == 0 {
                self.output.extend_from_slice(b"\ncc: program exceeded its instruction budget\n");
                break;
            }
            budget -= 1;
            let ins = self.program.code[self.pc].clone();
            self.pc += 1;
            match ins {
                Ins::Push(value) => self.push(value),
                Ins::LoadLocal(offset) => {
                    let address = self.fp + offset as i64;
                    self.push(self.read_word(address as usize));
                }
                Ins::StoreLocal(offset) => {
                    let value = self.pop();
                    let address = (self.fp + offset as i64) as usize;
                    self.write_word(address, value);
                }
                Ins::LocalAddr(offset) => self.push(self.fp + offset as i64),
                Ins::LoadGlobal(address) => self.push(self.read_word(address as usize)),
                Ins::LoadMem => {
                    let address = self.pop() as usize;
                    self.push(self.read_word(address));
                }
                Ins::StoreMem => {
                    let value = self.pop();
                    let address = self.pop() as usize;
                    self.write_word(address, value);
                }
                Ins::StoreMemKeep => {
                    let value = self.pop();
                    let address = self.pop() as usize;
                    self.write_word(address, value);
                    self.push(value);
                }
                Ins::LoadChar => {
                    let address = self.pop() as usize;
                    self.push(self.read_byte(address) as i64);
                }
                Ins::StoreChar => {
                    let value = self.pop();
                    let address = self.pop() as usize;
                    self.write_byte(address, value as u8);
                }
                Ins::StoreCharKeep => {
                    let value = self.pop();
                    let address = self.pop() as usize;
                    self.write_byte(address, value as u8);
                    self.push(value);
                }
                Ins::Swap => {
                    let top = self.stack.len();
                    if top >= 2 {
                        self.stack.swap(top - 1, top - 2);
                    }
                }
                Ins::Pop => {
                    self.pop();
                }
                Ins::Dup => {
                    let value = *self.stack.last().unwrap_or(&0);
                    self.push(value);
                }
                Ins::Add => self.arith(|a, b| a.wrapping_add(b)),
                Ins::Sub => self.arith(|a, b| a.wrapping_sub(b)),
                Ins::Mul => self.arith(|a, b| a.wrapping_mul(b)),
                Ins::Div => self.arith(|a, b| if b == 0 { 0 } else { a.wrapping_div(b) }),
                Ins::Mod => self.arith(|a, b| if b == 0 { 0 } else { a.wrapping_rem(b) }),
                Ins::Neg => {
                    let value = self.pop();
                    self.push(value.wrapping_neg());
                }
                Ins::And => self.arith(|a, b| a & b),
                Ins::Or => self.arith(|a, b| a | b),
                Ins::Xor => self.arith(|a, b| a ^ b),
                Ins::Not => {
                    let value = self.pop();
                    self.push(!value);
                }
                Ins::Shl => self.arith(|a, b| a.wrapping_shl((b & 63) as u32)),
                Ins::Shr => self.arith(|a, b| a.wrapping_shr((b & 63) as u32)),
                Ins::Eq => self.compare(|a, b| a == b),
                Ins::Ne => self.compare(|a, b| a != b),
                Ins::Lt => self.compare(|a, b| a < b),
                Ins::Le => self.compare(|a, b| a <= b),
                Ins::Gt => self.compare(|a, b| a > b),
                Ins::Ge => self.compare(|a, b| a >= b),
                Ins::Jump(target) => self.pc = target,
                Ins::JumpZ(target) => {
                    if self.pop() == 0 {
                        self.pc = target;
                    }
                }
                Ins::JumpNZ(target) => {
                    if self.pop() != 0 {
                        self.pc = target;
                    }
                }
                Ins::Call(index, argc) => self.call_function(index, argc),
                Ins::Builtin(id, argc) => self.builtin(id, argc),
                Ins::Ret => {
                    let value = self.pop();
                    if let Some((return_pc, saved_fp)) = self.call.pop() {
                        self.fp = saved_fp;
                        self.pc = return_pc;
                        self.push(value);
                    } else {
                        // Returning from the entry function: stop.
                        self.exit = Some(value);
                        break;
                    }
                }
                Ins::Halt => {
                    self.exit = Some(self.pop());
                    break;
                }
            }
            if let Some(_) = self.exit {
                break;
            }
        }
    }

    fn call_function(&mut self, index: usize, argc: u32) {
        let Some(info) = self.program.funcs.get(index).cloned() else {
            self.exit = Some(1);
            return;
        };
        let new_fp = (self.fp - info.frame as i64) & !15;
        if new_fp <= self.heap as i64 + 64 {
            self.output.extend_from_slice(b"\ncc: call stack overflow\n");
            self.exit = Some(1);
            return;
        }
        for i in (0..argc as usize).rev() {
            let value = self.pop();
            self.write_word((new_fp + (i as i64) * 8) as usize, value);
        }
        self.call.push((self.pc, self.fp));
        self.fp = new_fp;
        self.pc = info.entry;
    }

    // -- Builtins -----------------------------------------------------------

    fn builtin(&mut self, id: u32, argc: u32) {
        let mut args = Vec::with_capacity(argc as usize);
        for _ in 0..argc {
            args.push(self.pop());
        }
        args.reverse();
        let result = match id {
            builtin::PUTCHAR => {
                let c = args.first().copied().unwrap_or(0);
                self.output.push(c as u8);
                c
            }
            builtin::PRINT => {
                self.print_cstr(args.first().copied().unwrap_or(0) as usize);
                0
            }
            builtin::PUTS => {
                self.print_cstr(args.first().copied().unwrap_or(0) as usize);
                self.output.push(b'\n');
                0
            }
            builtin::PRINTF => {
                self.printf(&args);
                0
            }
            builtin::STRLEN => self.cstr_len(args.first().copied().unwrap_or(0) as usize) as i64,
            builtin::STRCMP => self.strcmp(
                args.first().copied().unwrap_or(0) as usize,
                args.get(1).copied().unwrap_or(0) as usize,
            ),
            builtin::STRCPY => {
                let dst = args.first().copied().unwrap_or(0) as usize;
                let src = args.get(1).copied().unwrap_or(0) as usize;
                self.strcpy(dst, src);
                dst as i64
            }
            builtin::STRCAT => {
                let dst = args.first().copied().unwrap_or(0) as usize;
                let src = args.get(1).copied().unwrap_or(0) as usize;
                let end = dst + self.cstr_len(dst);
                self.strcpy(end, src);
                dst as i64
            }
            builtin::ATOI => self.atoi(args.first().copied().unwrap_or(0) as usize),
            builtin::ABS => args.first().copied().unwrap_or(0).wrapping_abs(),
            builtin::MIN => args.first().copied().unwrap_or(0).min(args.get(1).copied().unwrap_or(0)),
            builtin::MAX => args.first().copied().unwrap_or(0).max(args.get(1).copied().unwrap_or(0)),
            builtin::MALLOC => self.malloc(args.first().copied().unwrap_or(0).max(0) as usize),
            builtin::FREE => 0,
            builtin::MEMSET => {
                let dst = args.first().copied().unwrap_or(0) as usize;
                let value = args.get(1).copied().unwrap_or(0) as u8;
                let count = args.get(2).copied().unwrap_or(0).max(0) as usize;
                for i in 0..count {
                    self.write_byte(dst + i, value);
                }
                dst as i64
            }
            builtin::MEMCPY => {
                let dst = args.first().copied().unwrap_or(0) as usize;
                let src = args.get(1).copied().unwrap_or(0) as usize;
                let count = args.get(2).copied().unwrap_or(0).max(0) as usize;
                for i in 0..count {
                    let byte = self.read_byte(src + i);
                    self.write_byte(dst + i, byte);
                }
                dst as i64
            }
            builtin::RAND => {
                self.rng = self.rng.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
                ((self.rng >> 33) & 0x7fff_ffff) as i64
            }
            builtin::SRAND => {
                self.rng = args.first().copied().unwrap_or(1) as u64;
                0
            }
            builtin::EXIT => {
                self.exit = Some(args.first().copied().unwrap_or(0));
                0
            }
            builtin::PRINT_INT => {
                let value = args.first().copied().unwrap_or(0);
                self.push_int(value);
                0
            }
            _ => 0,
        };
        self.push(result);
    }

    // -- Output helpers -----------------------------------------------------

    fn print_cstr(&mut self, address: usize) {
        let mut i = address;
        loop {
            let byte = self.read_byte(i);
            if byte == 0 {
                break;
            }
            self.output.push(byte);
            i += 1;
        }
    }

    fn cstr_len(&self, address: usize) -> usize {
        let mut i = address;
        while i < MEM_SIZE && self.mem[i] != 0 {
            i += 1;
        }
        i - address
    }

    fn strcmp(&self, a: usize, b: usize) -> i64 {
        let mut i = 0;
        loop {
            let x = self.read_byte(a + i) as i64;
            let y = self.read_byte(b + i) as i64;
            if x != y {
                return x - y;
            }
            if x == 0 {
                return 0;
            }
            i += 1;
        }
    }

    fn strcpy(&mut self, dst: usize, src: usize) {
        let mut i = 0;
        loop {
            let byte = self.read_byte(src + i);
            self.write_byte(dst + i, byte);
            if byte == 0 {
                break;
            }
            i += 1;
        }
    }

    fn atoi(&self, address: usize) -> i64 {
        let mut i = address;
        let mut sign = 1i64;
        let mut value = 0i64;
        let first = self.read_byte(i);
        if first == b'-' {
            sign = -1;
            i += 1;
        } else if first == b'+' {
            i += 1;
        }
        loop {
            let byte = self.read_byte(i);
            if !byte.is_ascii_digit() {
                break;
            }
            value = value * 10 + (byte - b'0') as i64;
            i += 1;
        }
        value * sign
    }

    fn malloc(&mut self, size: usize) -> i64 {
        let base = (self.heap + 15) & !15;
        let end = base + size.max(1);
        if end >= MEM_SIZE / 2 {
            return 0;
        }
        self.heap = end;
        base as i64
    }

    fn printf(&mut self, args: &[i64]) {
        let address = args.first().copied().unwrap_or(0).max(0) as usize;
        let mut i = address;
        let mut next = 1usize;
        loop {
            let c = self.read_byte(i);
            if c == 0 {
                break;
            }
            if c != b'%' {
                self.output.push(c);
                i += 1;
                continue;
            }
            i += 1;
            // Skip flags, width and precision; they are honoured nowhere.
            while self.read_byte(i).is_ascii_digit()
                || matches!(self.read_byte(i), b'-' | b'+' | b' ' | b'#' | b'.' | b'0')
            {
                i += 1;
            }
            while matches!(self.read_byte(i), b'l' | b'h' | b'z' | b'j' | b't') {
                i += 1;
            }
            let spec = self.read_byte(i);
            i += 1;
            let value = if spec == b'%' {
                0
            } else {
                let value = args.get(next).copied().unwrap_or(0);
                next += 1;
                value
            };
            match spec {
                b'd' | b'i' => self.push_int(value),
                b'u' => self.push_uint(value as u64),
                b'x' => self.push_hex(value as u64, false),
                b'X' => self.push_hex(value as u64, true),
                b'c' => self.output.push(value as u8),
                b's' => self.print_cstr(value.max(0) as usize),
                b'%' => self.output.push(b'%'),
                _ => {
                    self.output.push(b'%');
                    self.output.push(spec);
                }
            }
        }
    }

    fn push_int(&mut self, value: i64) {
        if value < 0 {
            self.output.push(b'-');
            self.push_uint(value.unsigned_abs());
        } else {
            self.push_uint(value as u64);
        }
    }

    fn push_uint(&mut self, mut value: u64) {
        if value == 0 {
            self.output.push(b'0');
            return;
        }
        let mut digits = [0u8; 20];
        let mut len = 0;
        while value > 0 {
            digits[len] = b'0' + (value % 10) as u8;
            value /= 10;
            len += 1;
        }
        for i in (0..len).rev() {
            self.output.push(digits[i]);
        }
    }

    fn push_hex(&mut self, mut value: u64, upper: bool) {
        let digits = if upper { b"0123456789ABCDEF" } else { b"0123456789abcdef" };
        let mut buffer = [0u8; 16];
        let mut len = 0;
        if value == 0 {
            self.output.push(b'0');
            return;
        }
        while value > 0 {
            buffer[len] = digits[(value & 0xf) as usize];
            value >>= 4;
            len += 1;
        }
        for i in (0..len).rev() {
            self.output.push(buffer[i]);
        }
    }

    // -- Memory and stack ---------------------------------------------------

    fn push(&mut self, value: i64) {
        self.stack.push(value);
    }

    fn pop(&mut self) -> i64 {
        self.stack.pop().unwrap_or(0)
    }

    fn arith(&mut self, op: impl Fn(i64, i64) -> i64) {
        let b = self.pop();
        let a = self.pop();
        self.push(op(a, b));
    }

    fn compare(&mut self, op: impl Fn(i64, i64) -> bool) {
        let b = self.pop();
        let a = self.pop();
        self.push(op(a, b) as i64);
    }

    fn read_byte(&self, address: usize) -> u8 {
        *self.mem.get(address).unwrap_or(&0)
    }

    fn write_byte(&mut self, address: usize, value: u8) {
        if address < MEM_SIZE {
            self.mem[address] = value;
        }
    }

    fn read_word(&self, address: usize) -> i64 {
        if address + 8 > MEM_SIZE {
            return 0;
        }
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&self.mem[address..address + 8]);
        i64::from_le_bytes(bytes)
    }

    fn write_word(&mut self, address: usize, value: i64) {
        if address + 8 > MEM_SIZE {
            return;
        }
        self.mem[address..address + 8].copy_from_slice(&value.to_le_bytes());
    }
}
