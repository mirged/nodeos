//! The PS/2 controller: one 8042, two devices, two vectors.
//!
//! Every PC has one of these, and it is the only piece of the machine on which
//! NodeOS drives *two* devices through a single pair of ports. The keyboard
//! and the mouse share a data port (0x60) and a status/command port (0x64),
//! and the only thing that tells them apart is one bit of the status register.
//! Getting that bit wrong is the classic PS/2 bug: a mouse packet delivered to
//! the keyboard decoder, or the reverse, and a system that looks like it is
//! dropping input at random.
//!
//! So there is exactly one place that reads the data port -- [`service`] --
//! and it decides which device the byte belongs to from the status register
//! rather than from which interrupt line happened to fire. Both vectors call
//! it. Whichever line the 8042 raises, the queue that byte lands on is the one
//! the hardware says it belongs to.
//!
//! The controller is also the only device in this tree the kernel initialises
//! by hand. The firmware leaves it in *some* state, and the mouse is not part
//! of that state in any reliable way: the aux port has to be enabled, its
//! clock and interrupt line set in the controller's configuration byte, and
//! the mouse itself asked to report data. `init` does all three, and the
//! keyboard's settings are preserved bit for bit around it -- the keyboard is
//! already working, and an init that reconfigured it on the way past would be
//! a driver that breaks one device to bring up another.
//!
//! See `docs/COMPOSITOR.md` for what the other end of the pointer stream does
//! with it.

use alloc::sync::Arc;

use super::port;
use crate::obj::Device;
use crate::sync::IrqSpinLock;

/// Data port: bytes in both directions.
const DATA_PORT: u16 = 0x60;
/// Status register when read, command register when written.
const STATUS_PORT: u16 = 0x64;

/// Status bit: the output buffer holds a byte worth reading.
const STATUS_OUTPUT_FULL: u8 = 1 << 0;
/// Status bit: the input buffer is still full, so writing now would lose the
/// byte.
const STATUS_INPUT_FULL: u8 = 1 << 1;
/// Status bit: the byte in the output buffer came from the *mouse*.
///
/// This one bit is the entire device discrimination in this module.
const STATUS_FROM_AUX: u8 = 1 << 5;

/// Controller command: read the configuration byte.
const CMD_READ_CONFIG: u8 = 0x20;
/// Controller command: write the configuration byte.
const CMD_WRITE_CONFIG: u8 = 0x60;
/// Controller command: enable the auxiliary (mouse) port.
const CMD_ENABLE_AUX: u8 = 0xA8;
/// Controller command: disable the auxiliary port.
const CMD_DISABLE_AUX: u8 = 0xA7;
/// Controller command: enable the keyboard port.
const CMD_ENABLE_KEYBOARD: u8 = 0xAE;
/// Controller command: disable the keyboard port.
const CMD_DISABLE_KEYBOARD: u8 = 0xAD;
/// Controller command: route the next data byte to the mouse.
const CMD_WRITE_TO_AUX: u8 = 0xD4;

/// Configuration bit: deliver interrupts from the keyboard port.
const CONFIG_IRQ1: u8 = 1 << 0;
/// Configuration bit: deliver interrupts from the auxiliary port.
const CONFIG_IRQ12: u8 = 1 << 1;
/// Configuration bit: the keyboard clock is *disabled*.
const CONFIG_KEYBOARD_CLOCK_OFF: u8 = 1 << 4;
/// Configuration bit: the auxiliary clock is *disabled*.
const CONFIG_AUX_CLOCK_OFF: u8 = 1 << 5;

/// Mouse command: load the default resolution, sample rate and scaling.
const MOUSE_SET_DEFAULTS: u8 = 0xF6;
/// Mouse command: start reporting movement.
const MOUSE_ENABLE_REPORTING: u8 = 0xF4;
/// Every PS/2 device acknowledges a command with this byte.
const DEVICE_ACK: u8 = 0xFA;

/// How many times to poll a controller handshake before giving up.
///
/// The 8042 is a slow, ancient part and the loops below are only ever waiting
/// for `inb` to change; a few hundred thousand polls is already several
/// milliseconds on any CPU this kernel runs on. What matters is that the wait
/// *ends*: a boot that hangs when somebody plugs in a mouse the controller
/// cannot talk to is worse than a boot that comes up without a pointer.
const HANDSHAKE_LIMIT: u32 = 200_000;

/// Upper bound on bytes drained in one [`service`] call.
///
/// A machine that kept reporting output-full forever -- a wedged controller,
/// or a device that talks without being asked -- must not be able to keep this
/// CPU in an interrupt handler. 64 bytes is more than a full mouse packet plus
/// a burst of typing, which is what one interrupt actually has to carry.
const SERVICE_LIMIT: usize = 64;

/// The keyboard `Device`, once one exists to receive bytes.
///
/// Raw statics rather than per-CPU slots: there is one PS/2 controller per
/// machine, both its interrupt lines are only ever delivered to the boot CPU
/// in this kernel (there is no I/O APIC to route them elsewhere), and both are
/// only ever touched from an interrupt handler or from single-threaded
/// bring-up. `IrqSpinLock` keeps that honest on the way past.
static KEYBOARD_DEVICE: IrqSpinLock<Option<Arc<Device>>> = IrqSpinLock::new(None);
/// The mouse `Device`. See [`KEYBOARD_DEVICE`].
static MOUSE_DEVICE: IrqSpinLock<Option<Arc<Device>>> = IrqSpinLock::new(None);

/// Wait until the controller will accept a command or data byte.
fn wait_for_input_room() -> bool {
    for _ in 0..HANDSHAKE_LIMIT {
        // SAFETY: reading the status port has no side effects.
        if unsafe { port::inb(STATUS_PORT) } & STATUS_INPUT_FULL == 0 {
            return true;
        }
        // SAFETY: a port read of a fixed ISA port, with no memory effects.
        unsafe { port::io_wait() };
    }
    false
}

/// Wait until the controller has a byte for the host, and read it.
fn read_byte() -> Option<u8> {
    for _ in 0..HANDSHAKE_LIMIT {
        // SAFETY: reading the status port has no side effects.
        if unsafe { port::inb(STATUS_PORT) } & STATUS_OUTPUT_FULL != 0 {
            // SAFETY: the output buffer is full, so this read consumes a byte
            // the controller has for us rather than returning stale data.
            return Some(unsafe { port::inb(DATA_PORT) });
        }
        // SAFETY: as above -- a port read with no memory effects.
        unsafe { port::io_wait() };
    }
    None
}

/// Throw away anything the controller has buffered.
fn flush() {
    for _ in 0..SERVICE_LIMIT {
        // SAFETY: reading the status port has no side effects.
        if unsafe { port::inb(STATUS_PORT) } & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // SAFETY: as `read_byte`.
        unsafe { port::inb(DATA_PORT) };
    }
}

/// Send a command to the controller itself.
fn write_command(command: u8) -> bool {
    if !wait_for_input_room() {
        return false;
    }
    // SAFETY: the input buffer is empty, so this byte lands in the command
    // register and is not lost.
    unsafe { port::outb(STATUS_PORT, command) };
    true
}

/// Send a data byte to whichever device the controller is addressing.
fn write_data(byte: u8) -> bool {
    if !wait_for_input_room() {
        return false;
    }
    // SAFETY: the input buffer is empty; this byte goes to the device
    // selected by the preceding command.
    unsafe { port::outb(DATA_PORT, byte) };
    true
}

/// Send one command byte to the mouse and wait for its acknowledgement.
fn mouse_command(command: u8) -> bool {
    if !write_command(CMD_WRITE_TO_AUX) || !write_data(command) {
        return false;
    }
    // A byte should come straight back -- normally `0xFA`. Anything else is
    // either a device that does not speak this protocol or a byte left over
    // from an earlier command; either way this is not an acknowledgement, and
    // the caller is told so rather than being lied to.
    read_byte() == Some(DEVICE_ACK)
}

/// Bring the controller up, probe the mouse, and start both data streams.
///
/// Returns whether the mouse answered. A `false` return is not a failure --
/// plenty of machines have no PS/2 mouse -- but a driver that mistook one for
/// a working device would wait on a byte stream that will never fill.
///
/// The keyboard is left exactly as it was found: the configuration byte is
/// read, two interrupt lines and two clock-enables are adjusted, and every
/// other bit is written back unchanged, because the keyboard is already
/// working before this runs and must still work after it.
///
/// # Safety
/// Call once, on the boot CPU, before any handler reads the data port. Takes
/// exclusive ownership of ports 0x60 and 0x64: with IRQ1 or IRQ12 unmasked,
/// a handler reading the data port concurrently would steal bytes from the
/// handshakes below.
pub unsafe fn init() -> bool {
    // Stop the devices talking while the controller is reconfigured, and
    // discard whatever the firmware left in the buffers -- a stale byte read
    // as an acknowledgement is exactly the kind of thing that makes a probe
    // report success on a machine with no mouse attached.
    write_command(CMD_DISABLE_KEYBOARD);
    write_command(CMD_DISABLE_AUX);
    flush();

    // Read, adjust, write back. Failing to read the byte is not a reason to
    // guess at a configuration: fall back to the power-on value, which has
    // both devices enabled and both interrupts on.
    let config = if write_command(CMD_READ_CONFIG) { read_byte().unwrap_or(0x00) } else { 0x00 };
    let config = (config | CONFIG_IRQ1 | CONFIG_IRQ12) & !(CONFIG_KEYBOARD_CLOCK_OFF | CONFIG_AUX_CLOCK_OFF);
    let program_ok = write_command(CMD_WRITE_CONFIG) && write_data(config);

    write_command(CMD_ENABLE_KEYBOARD);
    write_command(CMD_ENABLE_AUX);
    flush();

    // Now the mouse itself. `0xF6` first so the probe does not depend on the
    // sample rate or resolution a previous owner left behind, then reporting.
    let mouse_present = program_ok && mouse_command(MOUSE_SET_DEFAULTS) && mouse_command(MOUSE_ENABLE_REPORTING);

    crate::info!(
        "ps2: controller programmed (config {config:#04x}), mouse {}",
        if mouse_present { "present" } else { "absent" }
    );
    mouse_present
}

/// Give the controller the two objects its interrupts feed.
///
/// # Safety
/// Call once, with interrupts disabled or with both vectors still masked.
pub unsafe fn attach(keyboard: Arc<Device>, pointer: Arc<Device>) {
    *KEYBOARD_DEVICE.lock() = Some(keyboard);
    *MOUSE_DEVICE.lock() = Some(pointer);
}

/// Read everything the controller has for the host and queue it on the device
/// it came from.
///
/// Called from the interrupt handler for both PS/2 vectors, with interrupts
/// disabled. Never blocks and never allocates.
pub fn service() {
    for _ in 0..SERVICE_LIMIT {
        // SAFETY: reading the status port has no side effects.
        let status = unsafe { port::inb(STATUS_PORT) };
        if status & STATUS_OUTPUT_FULL == 0 {
            return;
        }
        // SAFETY: the output buffer is full, so this read consumes the byte
        // the status register is describing -- which is what makes the status
        // bit and the data byte below refer to the same event.
        let byte = unsafe { port::inb(DATA_PORT) };

        let device = if status & STATUS_FROM_AUX != 0 {
            MOUSE_DEVICE.lock().clone()
        } else {
            KEYBOARD_DEVICE.lock().clone()
        };
        if let Some(device) = device {
            device.push_input(byte);
        }
    }
}
