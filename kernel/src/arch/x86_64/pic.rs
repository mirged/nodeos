//! Legacy 8259A programmable interrupt controller.
//!
//! NodeOS uses the PIC only to *disable* it. The vectors it delivers by
//! default collide with the CPU's architectural exceptions, so it must be
//! remapped before interrupts are enabled even if the machine will end up
//! using the APIC. Once the I/O APIC is up, the PIC is fully masked and left
//! alone -- except for the spurious-interrupt check, which still has to work.

use super::port;

const PIC1_COMMAND: u16 = 0x20;
const PIC1_DATA: u16 = 0x21;
const PIC2_COMMAND: u16 = 0xA0;
const PIC2_DATA: u16 = 0xA1;

const ICW1_INIT: u8 = 0x10;
const ICW1_ICW4: u8 = 0x01;
const ICW4_8086: u8 = 0x01;
const CMD_END_OF_INTERRUPT: u8 = 0x20;
const CMD_READ_ISR: u8 = 0x0B;

/// Vector base for master PIC IRQs 0..8.
pub const PIC1_VECTOR_BASE: u8 = 32;
/// Vector base for slave PIC IRQs 8..16.
pub const PIC2_VECTOR_BASE: u8 = 40;

/// Remap both PICs above the architectural exception range and mask every line.
///
/// # Safety
/// Must run with interrupts disabled. Takes exclusive ownership of ports
/// 0x20/0x21 and 0xA0/0xA1.
pub unsafe fn init() {
    // SAFETY: the standard ICW1..ICW4 initialisation sequence. `io_wait`
    // between writes gives the (notoriously slow) 8259 time to latch each word
    // on machines where the ISA bus is not self-throttling.
    unsafe {
        port::outb(PIC1_COMMAND, ICW1_INIT | ICW1_ICW4);
        port::io_wait();
        port::outb(PIC2_COMMAND, ICW1_INIT | ICW1_ICW4);
        port::io_wait();

        port::outb(PIC1_DATA, PIC1_VECTOR_BASE);
        port::io_wait();
        port::outb(PIC2_DATA, PIC2_VECTOR_BASE);
        port::io_wait();

        port::outb(PIC1_DATA, 1 << 2); // slave is wired to master IRQ2
        port::io_wait();
        port::outb(PIC2_DATA, 2); // slave identity
        port::io_wait();

        port::outb(PIC1_DATA, ICW4_8086);
        port::io_wait();
        port::outb(PIC2_DATA, ICW4_8086);
        port::io_wait();

        mask_all();
    }
}

/// Mask every IRQ line on both controllers.
///
/// # Safety
/// Requires ownership of the PIC ports.
pub unsafe fn mask_all() {
    // SAFETY: writing 0xFF to the data port masks all eight lines.
    unsafe {
        port::outb(PIC1_DATA, 0xFF);
        port::outb(PIC2_DATA, 0xFF);
    }
}

/// Unmask a single IRQ line (0..16).
///
/// # Safety
/// A handler for the corresponding vector must already be installed.
pub unsafe fn unmask(irq: u8) {
    let (data_port, bit) = if irq < 8 { (PIC1_DATA, irq) } else { (PIC2_DATA, irq - 8) };
    // SAFETY: read-modify-write of the interrupt mask register.
    unsafe {
        let mask = port::inb(data_port) & !(1 << bit);
        port::outb(data_port, mask);
        if irq >= 8 {
            // The cascade line must be open for slave IRQs to arrive at all.
            let mask = port::inb(PIC1_DATA) & !(1 << 2);
            port::outb(PIC1_DATA, mask);
        }
    }
}

/// Mask a single IRQ line.
///
/// # Safety
/// Requires ownership of the PIC ports.
pub unsafe fn mask(irq: u8) {
    let (data_port, bit) = if irq < 8 { (PIC1_DATA, irq) } else { (PIC2_DATA, irq - 8) };
    // SAFETY: read-modify-write of the interrupt mask register.
    unsafe {
        let mask = port::inb(data_port) | (1 << bit);
        port::outb(data_port, mask);
    }
}

/// Signal end-of-interrupt for `vector`.
///
/// # Safety
/// Must only be called from the handler for that vector.
pub unsafe fn end_of_interrupt(vector: u8) {
    // SAFETY: EOI is the architecturally required acknowledgement.
    unsafe {
        if vector >= PIC2_VECTOR_BASE {
            port::outb(PIC2_COMMAND, CMD_END_OF_INTERRUPT);
        }
        port::outb(PIC1_COMMAND, CMD_END_OF_INTERRUPT);
    }
}

/// Whether `vector` is a genuine interrupt rather than an 8259 spurious one.
///
/// IRQ 7 and IRQ 15 can be raised by line noise. The controller reports them
/// but does not set the corresponding In-Service Register bit, and a spurious
/// interrupt must *not* be acknowledged with EOI.
///
/// # Safety
/// Requires ownership of the PIC ports.
pub unsafe fn is_real(vector: u8) -> bool {
    let irq = vector.wrapping_sub(PIC1_VECTOR_BASE);
    if irq != 7 && irq != 15 {
        return true;
    }
    let command_port = if irq == 7 { PIC1_COMMAND } else { PIC2_COMMAND };
    let bit = if irq == 7 { 7 } else { 7 };
    // SAFETY: OCW3 selects the ISR for the next read of the command port.
    unsafe {
        port::outb(command_port, CMD_READ_ISR);
        let isr = port::inb(command_port);
        if isr & (1 << bit) != 0 {
            true
        } else {
            // Spurious slave interrupts still need the master acknowledged.
            if irq == 15 {
                port::outb(PIC1_COMMAND, CMD_END_OF_INTERRUPT);
            }
            false
        }
    }
}
