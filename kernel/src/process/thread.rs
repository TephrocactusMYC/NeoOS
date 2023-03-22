//!  An executing program consists of a collection of native OS threads, each with their
//! own stack and local state. Threads can be named, and provide some built-in support for
//! low-level synchronization.
//!
//! Communication between threads can be done through signals, shared memory, along with
//! other forms of thread synchronization and shared-memory data structures. In particular,
//! types that are guaranteed to be threadsafe are easily shared between threads using the
//! atomically-reference-counted container, Arc.

use core::{fmt::Debug, time::Duration};

use alloc::{
    boxed::Box,
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Weak},
    vec::Vec,
};
use lazy_static::lazy_static;
use spin::RwLock;

use crate::{
    arch::{
        cpu::{cpu_id, FpState, MAX_CPU_NUM},
        interrupt::{dispatcher::trap_dispatcher_user, Context},
        mm::paging::{KernelPageTable, PageTableBehaviors},
        pit::countdown,
        PAGE_SIZE,
    },
    elf::ElfFile,
    error::{fserror_to_kerror, Errno, KResult},
    fs::ROOT_INODE,
    memory::{KernelFrameAllocator, USER_STACK_SIZE, USER_STACK_START},
    mm::{callback::SystemArenaCallback, Arena, ArenaFlags, FutureWithPageTable, MemoryManager},
    signal::{handle_signal, SigAction, SigSet, SigStack},
    sync::mutex::SpinLockNoInterrupt as Mutex,
};

use super::{event::EventBus, register, scheduler::FIFO_SCHEDULER, Process, Yield};

const DEBUG_THREAD_ID: u64 = 0xdeadbeef;
const DEBUG_PROC_ID: u64 = 0xbeefdead;

/// A naked function that is used to test if ring switch works. If it works, this function would trigger general
/// protection fault (0xd) indicating that `hlt` is privileged instruction so that the user-level application is
/// expected to abort immediately.
///
/// Since we haven't implemented filesystem, we cannot load the program from the filesystem. So, the solution for
/// the time being is, to simply load the instruction from the memory and make that memory user-accessible.
#[naked]
#[allow(unused)]
unsafe extern "C" fn __debug_thread_invalid() {
    unsafe { core::arch::asm!("hlt", options(noreturn)) }
}

#[naked]
#[allow(unused)]
unsafe extern "C" fn __debug_thread() {
    unsafe { core::arch::asm!("int3; lea rax, [rip]; jmp rax", options(noreturn)) }
}

/// An enum representing the state of a thread.
///
/// This enum has five variants: `RUNNING`, `WAITING`, `SLEEPING`, `STOPPED`, and `ZOMBIE`.
///
/// # Examples
///
/// ```
/// # use kernel::process::thread::ThreadState;
/// let state = ThreadState::RUNNING;
/// println!("The thread is currently {:?}", state);
/// ```
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ThreadState {
    /// The thread is currently being executed by the processor.
    RUNNING,
    /// The thread is waiting for a resource or event to become available.
    WAITING,
    /// The thread is sleeping for a specified amount of time.
    SLEEPING,
    /// The thread has been stopped by a signal or other external event.
    STOPPED,
    /// The thread has terminated but its parent process has not yet waited on it.
    ZOMBIE,
}

pub struct Thread {
    /// The thread id.
    pub id: u64,
    /// The parent process.
    pub parent: Arc<Mutex<Process>>,
    /// The inner thread context.
    pub inner: Arc<Mutex<ThreadInner>>,
    /// Proc.vm
    pub vm: Arc<Mutex<MemoryManager<KernelPageTable>>>,
}

/// Finds a free tid and assigns it to the current thread by `register`.
pub fn find_available_tid() -> KResult<u64> {
    (1u64..)
        .find(|id| THREAD_TABLE.read().get(id).is_none())
        .ok_or(Errno::EBUSY)
}

/// Put the current core into sleeping state and wake it up after `duration` time.
///
/// Note, however, that this operation can be interrupted by, e.g., a syscall. The target of this function is to ensure
/// a sleep in the current thread, but not the whole system; the current thread cannot make the system suspend.
pub fn sleep(duration: Duration) {
    // The granularity is 10 ms.
    const TIME_SLICE: u128 = 10_000;

    let mut cnt = (duration.as_millis() / TIME_SLICE).min(u64::MAX as u128) as u64;
    while cnt != 0 {
        countdown(TIME_SLICE as _);
        cnt -= 1;
    }
}

impl Thread {
    /// Prepares the user stack. Returns the stack top.
    fn prepare_user_stack(vm: &mut MemoryManager<KernelPageTable>) -> KResult<usize> {
        let user_stack_bottom = USER_STACK_START;
        let user_stack_top = USER_STACK_START + USER_STACK_SIZE;

        // Reserve 4 pages for init info.
        // This is because the execution of the ELF file must requrie argc, argc, envp things.
        let flags = ArenaFlags {
            user_accessible: true,
            non_executable: true,
            writable: true,
            ..Default::default()
        };

        // This stack is allocated for the user thread.
        vm.add(Arena {
            range: user_stack_bottom as u64..(user_stack_top - PAGE_SIZE * 4) as u64,
            flags: flags.clone(),
            callback: Box::new(SystemArenaCallback::new(KernelFrameAllocator)),
        });
        // This stack is allocated for storing the auxiliary information such as argv, envp, etc.
        vm.add(Arena {
            range: (user_stack_top - PAGE_SIZE * 4) as u64..user_stack_top as u64,
            flags,
            callback: Box::new(SystemArenaCallback::new(KernelFrameAllocator)),
        });

        Ok(user_stack_top)
    }

    /// Activates this thread and registers it to the global thread table `THREAD_TABLE`.
    pub fn register(mut self) -> KResult<Arc<Self>> {
        let mut table = THREAD_TABLE.write();

        let id = match self.id {
            0 => find_available_tid()?,
            id => id,
        };
        self.id = id;
        let arced_self = Arc::new(self);
        table.insert(id, arced_self.clone());

        Ok(arced_self)
    }

    /// Forks this thread.
    pub fn fork(&mut self, context: &Context) -> Arc<Self> {
        // Cow the vm.
        let vm = Arc::new(Mutex::new(self.vm.lock().clone()));

        let mut ctx = context.clone();
        ctx.regs.rax = 0;

        let mut lock = self.parent.lock();
        let id = find_available_tid().unwrap();
        let forked_process = Arc::new(Mutex::new(Process {
            process_id: id,
            process_group_id: lock.process_group_id,
            threads: Vec::new(),
            vm: vm.clone(),
            exec_path: lock.exec_path.clone(),
            pwd: lock.pwd.clone(),
            opened_files: BTreeMap::new(),
            exit_code: 0,
            event_bus: EventBus::new(),
            futexes: BTreeMap::new(),
            parent: (lock.process_id, Arc::downgrade(&self.parent)),
            children: Vec::new(),
            pending_sigset: SigSet::default(),
            sig_queue: VecDeque::new(),
            actions: lock.actions,
        }));

        register(&forked_process, id);

        let thread = Thread {
            id,
            parent: forked_process,
            inner: Arc::new(Mutex::new(ThreadInner {
                sigmask: self.inner.lock().sigmask,
                thread_context: Some(ThreadContext {
                    user_context: Box::new(ctx),
                    fp_state: Box::new(FpState::new()),
                }),
                sigaltstack: self.inner.lock().sigaltstack.clone(),
            })),
            vm,
        }
        .register()
        .unwrap();

        thread.parent.lock().threads.push(id);
        lock.children
            .push((thread.id, Arc::downgrade(&thread.parent)));

        thread
    }

    pub fn take(&self) -> ThreadContext {
        self.inner.lock().thread_context.take().unwrap()
    }

    pub fn restore(&self, ctx: ThreadContext) {
        self.inner.lock().thread_context.replace(ctx);
    }

    /// Creates a raw thread with in-memory instructions.
    /// Returns the user stack top.
    ///
    /// # Note
    ///
    /// Do *not directly* use this function unless you need to debug if kernel correctly handles multi-threading.
    ///
    /// # Safety
    ///
    /// This function is unsafe because `inst_addr` must be valid.
    pub unsafe fn from_raw() -> KResult<Arc<Thread>> {
        let mut vm: MemoryManager<KernelPageTable> = MemoryManager::new(false);
        let stack_top = Self::prepare_user_stack(&mut vm)? as u64;
        let elf_inode = ROOT_INODE.lookup("/bin/test").map_err(fserror_to_kerror)?;
        let elf = ElfFile::load(&elf_inode)?;
        elf.load_elf_and_map(&mut vm)?;
        let vm = Arc::new(Mutex::new(vm));

        // So we must pretend that 'interrupt' occurs here so that CPU allows to perform `IRETQ`.
        // To this end, we must carefully construct the stack upon return, whose layout should be:
        //
        // SS           <- stack selector before interrupt
        // RSP          <- stack pointer before interrupt
        // RFLAGS       <- flag register before interrupt
        // CS           <- code selector before interrupt
        // RIP          <- instruction pointer register before interupt
        // error_code   <- error code / trap number / syscall number
        // blahblah     <- SS:RSP
        let mut context = Context::default();

        kinfo!(
            "the ELF entry point is 0x{:x}; stack top is 0x{:x}",
            elf.entry_point(),
            stack_top
        );
        context.set_rip(elf.entry_point());
        context.set_rsp(stack_top);
        // IOPL | IF | RSVD
        context.regs.rflags = 0x3202;

        let thread = Thread {
            id: DEBUG_THREAD_ID,
            parent: Arc::new(Mutex::new(Process {
                process_id: DEBUG_PROC_ID,
                process_group_id: DEBUG_PROC_ID,
                threads: Vec::new(),
                vm: vm.clone(),
                exec_path: "".into(),
                pwd: ".".into(),
                opened_files: BTreeMap::new(),
                exit_code: 0u8,
                event_bus: EventBus::new(),
                futexes: BTreeMap::new(),
                parent: (0xffff_ffff, Weak::new()),
                children: Vec::new(),
                pending_sigset: SigSet::default(),
                sig_queue: VecDeque::new(),
                actions: [SigAction::default(); 0x41],
            })),
            inner: Arc::new(Mutex::new(ThreadInner {
                sigmask: SigSet::new(),
                thread_context: Some(ThreadContext {
                    user_context: Box::new(context),
                    fp_state: Box::new(FpState::new()),
                }),
                sigaltstack: SigStack::default(),
            })),
            vm,
        };

        // Add itself into the global thread table.
        let thread_ref = thread.register()?;
        register(&thread_ref.parent, DEBUG_PROC_ID);

        Ok(thread_ref)
    }
}

#[derive(Default)]
pub struct ThreadInner {
    /// Signals that this thread ignores.
    pub sigmask: SigSet,
    /// The thread context.
    pub thread_context: Option<ThreadContext>,
    /// The signal alternative stack.
    pub sigaltstack: SigStack,
}

/// A structure representing the context of a thread.
///
/// This struct contains two fields: `user_context` and `fp_state`.
/// `user_context` is a boxed [`Context`] struct, which represents the user-mode
/// state of the thread. `fp_state` is a boxed [`FpState`] struct, which represents
/// the floating-point state of the thread.
///
/// # Examples
///
/// ```
/// # use kernel::process::thread::ThreadContext;
/// let mut ctx = ThreadContext::new();
/// ```
pub struct ThreadContext {
    user_context: Box<Context>,
    fp_state: Box<FpState>,
}

impl ThreadContext {
    pub fn switch(&mut self) {
        self.fp_state.fxrstor();
        self.user_context.start();
        self.fp_state.fxsave();
    }

    pub fn get_trapno(&self) -> usize {
        self.user_context.trapno as usize
    }

    pub fn get_user_context(&mut self) -> &mut Box<Context> {
        &mut self.user_context
    }
}

impl Debug for ThreadContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ThreadContext")
            .field("User Context", self.user_context.as_ref())
            .field("Float Point State", self.fp_state.as_ref())
            .finish()
    }
}

pub static mut CURRENT_THREAD_PER_CPU: [Option<Arc<Thread>>; MAX_CPU_NUM] = {
    const THREAD: Option<Arc<Thread>> = None;
    [THREAD; MAX_CPU_NUM]
};

lazy_static! {
    pub static ref THREAD_TABLE: RwLock<BTreeMap<u64, Arc<Thread>>> = RwLock::new(BTreeMap::new());
}

/// Gets a handle to the thread that invokes it.
pub fn current() -> KResult<Arc<Thread>> {
    unsafe {
        Ok(CURRENT_THREAD_PER_CPU[cpu_id()]
            .as_ref()
            .ok_or(Errno::EEXIST)?
            .clone())
    }
}

/// This function spawns a new kernel thread from the given [`Thread`] object.
///
/// This function returns a [`KResult`] object, indicating whether the operation was successful. If the thread was
/// successfully spawned, Ok(()) is returned. If an error occurred, an appropriate error code is returned.
pub fn spawn(thread: Arc<Thread>) -> KResult<()> {
    let cr3 = thread.vm.lock().page_table().cr3();
    let thread_clone = thread.clone();
    let mut exited = false;
    let mut should_yield = false;

    let thread_future = async move {
        loop {
            let mut ctx = thread.take();
            // Perform a context switch.
            ctx.switch();

            // syscall / trap: anyway, a context switch happens here.
            if !trap_dispatcher_user(&thread, &mut ctx, &mut should_yield).await {
                // TODO: Elegantly kill the process and reclaim all the resources it occupies.
                kerror!(
                    "spawn(): cannot handle context switch. Dumped context is {:#x?}",
                    ctx.user_context
                );
                break;
            }

            // Handle signal or other errors.
            if !exited {
                exited = handle_signal(&thread, &mut ctx.user_context);
            }

            thread.restore(ctx);
            if exited {
                kinfo!("spawn(): thread {:#x} ended.", thread.id);
                break;
            }
            if should_yield {
                // Suspend execution until is ready.
                ktrace!("spawn(): thread {:#x} yields the CPU.", thread.id);
                Yield::default().await
            }
        }
    };

    // Yield <- ThreadFuture <- PageTable <- Scheduler
    FIFO_SCHEDULER.spawn(
        FutureWithPageTable::new(Box::pin(thread_future), cr3, thread_clone),
        None,
    );

    Ok(())
}

/// Spawn a debug thread with in-memory instructions.
pub fn debug_threading() {
    let thread = match unsafe { Thread::from_raw() } {
        Ok(thread) => thread,
        Err(errno) => {
            kerror!("spawning debug thread failed. Errno: {:?}", errno);
            return;
        }
    };

    spawn(thread).unwrap();
}
