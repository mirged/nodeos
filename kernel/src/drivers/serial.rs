//! 16550A UART driver, used as the kernel's debug console.
//!
//! This is the *only* device driver in the kernel. Every other driver in
//! NodeOS is a userspace service holding a `Device` capability. The serial
//! port is special-cased because the kernel must be able to report a panic
//! that happens before, or because of, userspace.

use core::fmt;

use crate::arch::x86_64::port;
use crate::sync::IrqSpinLock;

/// COM1 on every PC-compatible machine.
pub const COM1: u16 = 0x3F8;

// 16550 register offsets (with DLAB clear unless noted).
const REG_DATA: u16 = 0; // RBR/THR, or divisor low with DLAB set
const REG_INT_ENABLE: u16 = 1; // IER, or divisor high with DLAB set
const REG_FIFO_CTRL: u16 = 2; // FCR (write) / IIR (read)
const REG_LINE_CTRL: u16 = 3; // LCR
const REG_MODEM_CTRL: u16 = 4; // MCR
const REG_LINE_STATUS: u16 = 5; // LSR

const LSR_DATA_READY: u8 = 1 << 0;
const LSR_THR_EMPTY: u8 = 1 << 5;

const LCR_DLAB: u8 = 1 << 7;
const LCR_8N1: u8 = 0b11; // 8 data bits, no parity, 1 stop bit

/// A memory-less handle to a 16550 UART at a fixed I/O base.
pub struct Uart {
    base: u16,
    initialised: bool,
}

impl Uart {
    /// Create a handle. No I/O is performed until [`Uart::init`].
    pub const fn new(base: u16) -> Self {
        Self { base, initialised: false }
    }

    /// Program the UART for 115200 8N1 with FIFOs enabled.
    ///
    /// # Safety
    /// The caller must own the I/O port range `base..base+8`.
    pub unsafe fn init(&mut self) {
        // SAFETY: caller owns the port range. The sequence below is the
        // standard 16550 bring-up: mask interrupts, set the baud divisor
        // behind DLAB, restore the line control, enable and clear the FIFOs,
        // then assert DTR/RTS/OUT2.
        unsafe {
            port::outb(self.base + REG_INT_ENABLE, 0x00); // no interrupts yet
            port::outb(self.base + REG_LINE_CTRL, LCR_DLAB);
            port::outb(self.base + REG_DATA, 0x01); // divisor 1 => 115200 baud
            port::outb(self.base + REG_INT_ENABLE, 0x00); // divisor high
            port::outb(self.base + REG_LINE_CTRL, LCR_8N1); // clears DLAB
            port::outb(self.base + REG_FIFO_CTRL, 0xC7); // enable+clear FIFOs, 14-byte trigger
            port::outb(self.base + REG_MODEM_CTRL, 0x0B); // DTR | RTS | OUT2
        }
        self.initialised = true;
    }

    /// Enable the "received data available" interrupt.
    ///
    /// # Safety
    /// The IDT entry for the UART's IRQ line must already be installed.
    pub unsafe fn enable_rx_interrupt(&mut self) {
        // SAFETY: caller guarantees a handler exists.
        unsafe { port::outb(self.base + REG_INT_ENABLE, 0x01) };
    }

    fn line_status(&self) -> u8 {
        // SAFETY: LSR is a read-only status register; reading it clears only
        // sticky error bits we do not track.
        unsafe { port::inb(self.base + REG_LINE_STATUS) }
    }

    /// Write one byte, spinning until the transmit holding register is free.
    pub fn write_byte(&mut self, byte: u8) {
        if !self.initialised {
            return;
        }
        // Bounded spin: on real hardware the THR drains in ~87 us at 115200
        // baud; the cap keeps a dead UART from wedging the panic path.
        let mut spins = 0u32;
        while self.line_status() & LSR_THR_EMPTY == 0 {
            spins += 1;
            if spins > 1_000_000 {
                return;
            }
            core::hint::spin_loop();
        }
        // SAFETY: THR is empty, so this write cannot overrun the FIFO.
        unsafe { port::outb(self.base + REG_DATA, byte) };
    }

    /// Read one byte if the receiver has data.
    pub fn read_byte(&mut self) -> Option<u8> {
        if !self.initialised || self.line_status() & LSR_DATA_READY == 0 {
            return None;
        }
        // SAFETY: LSR reports a full receive buffer.
        Some(unsafe { port::inb(self.base + REG_DATA) })
    }
}

impl fmt::Write for Uart {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        for byte in s.bytes() {
            // The console is line-oriented; terminals want CRLF.
            if byte == b'\n' {
                self.write_byte(b'\r');
            }
            self.write_byte(byte);
        }
        Ok(())
    }
}

/// The kernel debug console.
pub static CONSOLE: IrqSpinLock<Uart> = IrqSpinLock::new(Uart::new(COM1));

/// Bring up the debug console. Called as early as possible in `kmain`.
///
/// # Safety
/// Must be called exactly once, before any logging, and only by the boot CPU.
pub unsafe fn init() {
    // SAFETY: the kernel owns COM1 until a userspace driver claims it, which
    // cannot happen before userspace exists.
    unsafe { CONSOLE.lock().init() };
}

/// Emergency console recovery for the panic path.
///
/// # Safety
/// Breaks mutual exclusion on the console. Only legal once the machine is
/// already committed to dying.
pub unsafe fn force_unlock() {
    // SAFETY: the panic path accepts the race in exchange for output.
    unsafe { CONSOLE.force_unlock() };
}
