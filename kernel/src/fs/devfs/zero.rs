//! /dev/zero A zerod device.

use core::sync::atomic::Ordering;

use rcore_fs::vfs::INode;

use super::INODE_COUNT;

pub struct ZeroINode {
    pub id: u64,
}

impl ZeroINode {
    pub fn new() -> Self {
        Self {
            id: INODE_COUNT.fetch_add(1, Ordering::SeqCst),
        }
    }
}

impl INode for ZeroINode {
    fn read_at(&self, offset: usize, buf: &mut [u8]) -> rcore_fs::vfs::Result<usize> {
        buf.fill(0);
        Ok(buf.len())
    }

    fn write_at(&self, offset: usize, buf: &[u8]) -> rcore_fs::vfs::Result<usize> {
        // Do not write to zero.
        Ok(0)
    }

    fn poll(&self) -> rcore_fs::vfs::Result<rcore_fs::vfs::PollStatus> {
        Ok(rcore_fs::vfs::PollStatus {
            read: true,
            write: false,
            error: false,
        })
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}
