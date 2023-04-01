//! Networking syscall interfaces.

use core::net::IpAddr;

use alloc::{boxed::Box, sync::Arc};

use crate::{
    arch::interrupt::SYSCALL_REGS_NUM,
    error::{Errno, KResult},
    fs::file::FileObject,
    net::{Socket, TcpStream, UdpStream},
    process::thread::{Thread, ThreadContext},
    sys::{IpProto, SockAddr, SocketOptions, SocketType, AF_INET, AF_UNIX},
    utils::ptr::Ptr,
};

/// `socket()` creates an endpoint for communication and returns a file descriptor that refers to that endpoint.
/// The file descriptor returned by a successful call will be the lowest-numbered file descriptor not currently
/// open for the process.
pub fn sys_socket(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    // The domain argument specifies a communication domain; this
    // selects the protocol family which will be used for communication.
    let domain = syscall_registers[0];
    // The socket has the indicated type, which specifies the
    // communication semantics.
    let ty = syscall_registers[1];
    // The protocol specifies a particular protocol to be used with the
    // socket.
    let protocol = syscall_registers[2];

    // FIXME: We assume they are valid for the time being.
    let socket_type = unsafe { core::mem::transmute::<u8, SocketType>((ty & 0xff) as u8) };
    let ipproto_type = unsafe { core::mem::transmute::<u8, IpProto>((protocol & 0xff) as u8) };

    let socket: Box<dyn Socket> = match domain {
        AF_INET | AF_UNIX => match socket_type {
            SocketType::SockStream => Box::new(TcpStream::new()),
            SocketType::SockDgram => Box::new(UdpStream::new()),
            SocketType::SockRaw => unimplemented!(),
        },

        _ => return Err(Errno::EINVAL), // unsupported.
    };

    let socket_fd = thread.parent.lock().add_file(FileObject::Socket(socket))?;
    Ok(socket_fd as _)
}

/// The `connect()` system call connects the socket referred to by the file descriptor sockfd to the address specified by addr.
/// The `addrlen` argument specifies the size of addr. The format of the address in addr is determined by the address space
/// of the socket sockfd.
pub fn sys_connect(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let socket_fd = syscall_registers[0];
    let sockaddr = syscall_registers[1];
    let addrlen = syscall_registers[2];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(socket_fd)?;

    if let FileObject::Socket(socket) = socket {
        // Parse the sockaddr and addrlen.
        let sockaddr_ptr = Ptr::new(sockaddr as *mut SockAddr);
        // FIXME: check_read is always wrong.
        // thread.vm.lock().check_read_array(&sockaddr_ptr, 1).unwrap();
        let addr = unsafe { sockaddr_ptr.read() }?;

        let ipv4_addr = match addr.sa_family as u64 {
            AF_INET => addr.to_core_sockaddr(),
            _ => return Err(Errno::EINVAL),
        };

        socket.connect(ipv4_addr).map(|_| 0)
    } else {
        Err(Errno::EBADF)
    }
}

/// Binds a name to a socket
pub fn sys_bind(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    let sockaddr = syscall_registers[1];
    let addrlen = syscall_registers[2];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;

    if let FileObject::Socket(socket) = socket {
        let sockaddr_ptr = Ptr::new(sockaddr as *mut SockAddr);
        let addr = unsafe { sockaddr_ptr.read() }?;

        let ipv4_addr = match addr.sa_family as u64 {
            AF_INET => addr.to_core_sockaddr(),
            _ => return Err(Errno::EINVAL),
        };

        socket.bind(ipv4_addr).map(|_| 0)
    } else {
        Err(Errno::EBADF)
    }
}

/// `listen()` marks the socket referred to by sockfd as a passive socket, that is, as a socket that will be used to accept
/// incoming connection requests using accept(2).
pub fn sys_listen(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    // Not used. always 0.
    let _backlog = syscall_registers[1];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;

    if let FileObject::Socket(socket) = socket {
        socket.listen().map(|_| 0)
    } else {
        Err(Errno::EBADF)
    }
}

/// The accept() system call is used with connection-based socket types (SOCK_STREAM, SOCK_SEQPACKET). It extracts the firs
/// connection request on the queue of pending connections for the listening socket, sockfd, creates a new connected socket
/// and returns a new file descriptor referring to that socket.  The newly created socket is not in the listening state.
/// The original socket sockfd is unaffected by this call.
pub fn sys_accept(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    let sockaddr = syscall_registers[1];
    let socklen = syscall_registers[2];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;

    if let FileObject::Socket(socket) = socket {
        let socket = socket
            .as_any_mut()
            .downcast_mut::<TcpStream>()
            .ok_or(Errno::EINVAL)?;
        let accepted = socket.accept()?;
        let peer = accepted.peer_addr().unwrap();

        if let IpAddr::V4(addr) = peer.ip() {
            // Write back to user space.
            let fd = proc.add_file(FileObject::Socket(accepted))?;
            let ptr = Ptr::new(sockaddr as *mut SockAddr);

            unsafe {
                ptr.write(SockAddr {
                    // Mark as fixed.
                    sa_family: AF_INET as _,
                    sa_data_min: {
                        let mut buf = [0u8; 14];
                        buf[..2].copy_from_slice(&peer.port().to_be_bytes());
                        buf[2..6].copy_from_slice(&addr.octets());
                        buf
                    },
                })?;
            }
            Ok(fd as _)
        } else {
            Err(Errno::EINVAL)
        }
    } else {
        Err(Errno::EBADF)
    }
}

///The setsockopt() function shall set the option specified by the option_name argument, at the protocol level specified by
/// the level argument, to the value pointed to by the option_value argument for the socket associated with the file
/// descriptor specified by the socket argument.
pub fn sys_setsockopt(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    let level = syscall_registers[1];
    let option_name = syscall_registers[2];
    let option_value = syscall_registers[3];
    let option_len = syscall_registers[4];

    let option_name = unsafe { core::mem::transmute::<u64, SocketOptions>(option_name) };
    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;

    if let FileObject::Socket(socket) = socket {
        // TODO: Check pointer.
        let value =
            unsafe { core::slice::from_raw_parts(option_value as *const u8, option_len as _) }
                .to_vec();
        socket.setsocketopt(option_name, value).map(|_| 0)
    } else {
        Err(Errno::EBADF)
    }
}

/// The system calls send(), sendto(), and sendmsg() are used to transmit a message to another socket.
///
/// # Note
///
/// There is no `send()` syscall because `send()` is converted to `sendto()`.
pub fn sys_sendto(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    let buf = syscall_registers[1];
    let len = syscall_registers[2];
    let flags = syscall_registers[3];
    let dst_addr = syscall_registers[4];
    let addr_len = syscall_registers[5];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;
    if let FileObject::Socket(socket) = socket {
        // Check if there is dst_addr.
        let dst_addr = Ptr::new(dst_addr as *mut SockAddr);
        let dst_addr = match dst_addr.is_null() {
            true => None,
            false => {
                let socket_address = unsafe { dst_addr.read() }?.to_core_sockaddr();
                Some(socket_address)
            }
        };

        // TODO: Check pointer.
        let buf = unsafe { core::slice::from_raw_parts(buf as *const u8, len as _) };
        socket.write(buf, dst_addr)
    } else {
        Err(Errno::EINVAL)
    }
}

/// The recv(), recvfrom(), and recvmsg() calls are used to receive messages from a socket. They may be used to receive data
/// on both connectionless and connection-oriented sockets. This page first describes common features of all three system
/// calls, and then describes the differences between the calls.
pub fn sys_recvfrom(
    thread: &Arc<Thread>,
    ctx: &mut ThreadContext,
    syscall_registers: [u64; SYSCALL_REGS_NUM],
) -> KResult<usize> {
    let sockfd = syscall_registers[0];
    let buf = syscall_registers[1];
    let len = syscall_registers[2];
    let flags = syscall_registers[3];
    let src_addr = syscall_registers[4];
    let addr_len = syscall_registers[5];

    let mut proc = thread.parent.lock();
    let socket = proc.get_fd(sockfd)?;
    if let FileObject::Socket(socket) = socket {
        let buf = unsafe { core::slice::from_raw_parts_mut(buf as *mut u8, len as _) };

        // TODO: We need to write to the `src_addr` (e.g., for UDP connections).
        socket.read(buf)
    } else {
        Err(Errno::EINVAL)
    }
}
