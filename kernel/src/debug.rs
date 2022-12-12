//! Implements all the utility functions related to debugging. Typically the `backtrace`
//! feature.
//!
//! This library is meant to supplement the `RUST_BACKTRACE=1` support of the
//! standard library by allowing an acquisition of a backtrace at runtime
//! programmatically. The backtraces generated by this library do not need to be
//! parsed, for example, and expose the functionality of multiple backend
//! implementations. This is because we have no backend.

use core::arch::asm;
use lazy_static::lazy_static;

use crate::{__guard_bottom, __guard_top, memory::check_within_stack};

lazy_static! {
    pub static ref UNWIND_DEPTH: usize = option_env!("RUST_BACKTRACE")
        .unwrap()
        .parse::<usize>()
        .unwrap_or(0x5);
}

/// Records backtrace-revelant register values.
#[derive(Debug)]
pub struct Frame {
    pub rip: u64,
    pub rsp: u64,
    pub rbp: u64,
}

impl Frame {
    /// Read the RSP register (stack pointer register).
    #[inline(always)]
    fn sp() -> u64 {
        let mut sp: u64;
        unsafe {
            asm!("mov {}, rsp", out(reg) sp);
        }
        sp
    }

    /// Reads the RBP register (stack base register).
    #[inline(always)]
    fn bp() -> u64 {
        let mut bp: u64;
        unsafe {
            asm!("mov {}, rbp", out(reg) bp);
        }
        bp
    }

    /// Reads the RIP register (instruction pointer).
    #[inline(always)]
    fn ip() -> u64 {
        let mut ip: u64;
        unsafe {
            asm!("lea {}, [rip]", out(reg) ip);
        }
        ip
    }

    /// Reads the return address (for the caller).
    #[inline(always)]
    fn ra(&self) -> u64 {
        let mut ra: u64;
        unsafe {
            asm!("mov 8({0}), {1}",
                 in(reg) self.rbp,
                 out(reg) ra,
                 options(att_syntax));
        }
        ra
    }

    /// Constructs a new `Frame` object that can be unwinded later.
    #[inline(always)]
    pub fn new() -> Self {
        Self {
            rsp: Self::sp(),
            rbp: Self::bp(),
            rip: Self::ip(),
        }
    }

    /// Unwinds this backtrace information based on the `depth`.
    ///
    /// To successfullly unwind the stack frame, the target specification file must specify
    /// ```json
    /// "frame-pointer": "always"
    /// ```
    /// to get this function work. Otherwise, the compiler will eliminate all RBPs if it thinks
    /// it is unnecessary to do so.
    pub fn unwind(&self, depth: usize) {
        let mut prev_ip = 0x0u64;
        let mut bp = self.rbp;
        // If bp is no longer in the stack address, we stop because it may want to acces bootloader
        // allocated memory region which is no longer valid.
        if !check_within_stack(bp) {
            log::error!(
                "\tNo stack trace available. Maybe this is the initial procedure. RBP was {:#x}",
                self.rbp
            );
            return;
        }

        let mut ip = self.ra();
        let mut cur_depth = 0;

        log::error!("=========== STACK BACKTRACE ===========");

        // Should prevent accesses to invalid addresses.
        while cur_depth != depth && ip <= __guard_top as u64 && ip >= __guard_bottom as u64 {
            if prev_ip != ip {
                // Print current situations.
                log::error!(
                    "\tStack #{:02x} - RIP: {:#018x} RBP: {:#018x}",
                    cur_depth,
                    ip - core::mem::size_of::<u64>() as u64,
                    bp,
                );

                prev_ip = ip;
                cur_depth += 1;
            }

            if !check_within_stack(bp) {
                break;
            }

            // Unwind the last function.
            ip = unsafe { *(bp as *const u64).add(1) };
            bp = unsafe { *(bp as *const u64) };
        }

        log::error!("=========== STACK BACKTRACE ===========");
    }
}

// With a raw libunwind pointer it should only ever be access in a readonly
// threadsafe fashion, so it's `Sync`. When sending to other threads via `Clone`
// we always switch to a version which doesn't retain interior pointers, so we
// should be `Send` as well.
unsafe impl Send for Frame {}
unsafe impl Sync for Frame {}
