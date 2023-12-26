use std::{
    ffi::CStr,
    io, mem,
    ops::{Deref, DerefMut},
    os::unix::io::RawFd,
    ptr,
};

use super::registrar::{UringFd, UringReadBuf, UringWriteBuf};

use nix::sys::socket::{SockaddrLike, SockaddrStorage};
pub use nix::{
    fcntl::{FallocateFlags, OFlag, PosixFadviseAdvice},
    poll::PollFlags,
    sys::{
        epoll::{EpollEvent, EpollOp},
        mman::MmapAdvise,
        socket::{MsgFlags, SockFlag},
        stat::Mode,
    },
};

use super::Personality;
use crate::{sys::Statx, uring_sys};

/// A pending IO event.
///
/// Can be configured with a set of
/// [`SubmissionFlags`](crate::sqe::SubmissionFlags).
pub struct SQE<'a> {
    sqe: &'a mut uring_sys::io_uring_sqe,
}

impl<'a> SQE<'a> {
    pub(crate) fn new(sqe: &'a mut uring_sys::io_uring_sqe) -> SQE<'a> {
        SQE { sqe }
    }

    /// Get this event's user data.
    #[inline]
    pub fn user_data(&self) -> u64 {
        self.sqe.user_data
    }

    /// Set this event's user data. User data is intended to be used by the
    /// application after completion.
    ///
    /// Note that you should not set user_data to `u64::MAX`. This value is
    /// reserved for timeouts generated by this library, setting an events'
    /// user_data to that value will cause the event's completion to
    /// swallowed by the library, and you will never find out that the event
    /// completed.
    ///
    /// # Safety
    ///
    /// This function is marked `unsafe`. The library from which you obtained
    /// this `SQE` may impose additional safety invariants which you must
    /// adhere to when setting the user_data for a submission queue event,
    /// which it may rely on when processing the corresponding completion
    /// queue event. For example, the library [ringbahn][ringbahn]
    ///
    /// # Example
    ///
    /// [ringbahn]: https://crates.io/crates/ringbahn
    pub unsafe fn set_user_data(&mut self, user_data: u64) {
        self.sqe.user_data = user_data as _;
    }

    /// Get this event's flags.
    #[inline]
    pub fn flags(&self) -> SubmissionFlags {
        SubmissionFlags::from_bits_retain(self.sqe.flags as _)
    }

    /// Overwrite this event's flags.
    pub fn overwrite_flags(&mut self, flags: SubmissionFlags) {
        self.sqe.flags = flags.bits() as _;
    }

    // must be called after any prep methods to properly complete mapped kernel IO
    #[inline]
    pub(crate) fn set_fixed_file(&mut self) {
        self.set_flags(SubmissionFlags::FIXED_FILE);
    }

    /// Set these flags for this event (any flags already set will still be
    /// set).
    #[inline]
    pub fn set_flags(&mut self, flags: SubmissionFlags) {
        self.sqe.flags |= flags.bits();
    }

    /// Set the [`Personality`] associated with this submission.
    #[inline]
    pub fn set_personality(&mut self, personality: Personality) {
        self.sqe.buf_index.buf_index.personality = personality.id;
    }

    /// Prepare a read on a file descriptor.
    ///
    /// Both the file descriptor and the buffer can be pre-registered. See the
    /// [`registrar][crate::registrar] module for more information.
    #[inline]
    pub unsafe fn prep_read(&mut self, fd: impl UringFd, buf: impl UringReadBuf, offset: u64) {
        buf.prep_read(fd, self, offset);
    }

    /// Prepare a vectored read on a file descriptor.
    #[inline]
    pub unsafe fn prep_read_vectored(
        &mut self,
        fd: impl UringFd,
        bufs: &mut [io::IoSliceMut<'_>],
        offset: u64,
    ) {
        let len = bufs.len();
        let addr = bufs.as_mut_ptr();
        uring_sys::io_uring_prep_readv(self.sqe, fd.as_raw_fd(), addr as _, len as _, offset as _);
        fd.update_sqe(self);
    }

    /// Prepare a read into a fixed, pre-registered buffer on a file descriptor.
    #[inline]
    pub unsafe fn prep_read_fixed(
        &mut self,
        fd: impl UringFd,
        buf: &mut [u8],
        offset: u64,
        buf_index: u32,
    ) {
        let len = buf.len();
        let addr = buf.as_mut_ptr();
        uring_sys::io_uring_prep_read_fixed(
            self.sqe,
            fd.as_raw_fd(),
            addr as _,
            len as _,
            offset as _,
            buf_index as _,
        );
        fd.update_sqe(self);
    }

    /// Prepare to write on a file descriptor.
    ///
    /// Both the file descriptor and the buffer can be pre-registered. See the
    /// [`registrar][crate::registrar] module for more information.
    #[inline]
    pub unsafe fn prep_write(&mut self, fd: impl UringFd, buf: impl UringWriteBuf, offset: u64) {
        buf.prep_write(fd, self, offset)
    }

    /// Prepare a vectored write on a file descriptor.
    #[inline]
    pub unsafe fn prep_write_vectored(
        &mut self,
        fd: impl UringFd,
        bufs: &[io::IoSlice<'_>],
        offset: u64,
    ) {
        let len = bufs.len();
        let addr = bufs.as_ptr();
        uring_sys::io_uring_prep_writev(self.sqe, fd.as_raw_fd(), addr as _, len as _, offset as _);
        fd.update_sqe(self);
    }

    /// Prepare to write on a file descriptor from a fixed, pre-registered
    /// buffer.
    #[inline]
    pub unsafe fn prep_write_fixed(
        &mut self,
        fd: impl UringFd,
        buf: &[u8],
        offset: u64,
        buf_index: usize,
    ) {
        let len = buf.len();
        let addr = buf.as_ptr();
        uring_sys::io_uring_prep_write_fixed(
            self.sqe,
            fd.as_raw_fd(),
            addr as _,
            len as _,
            offset as _,
            buf_index as _,
        );
        fd.update_sqe(self);
    }

    /// Prepare an fsync on a file descriptor.
    #[inline]
    pub unsafe fn prep_fsync(&mut self, fd: impl UringFd, flags: FsyncFlags) {
        uring_sys::io_uring_prep_fsync(self.sqe, fd.as_raw_fd(), flags.bits() as _);
        fd.update_sqe(self);
    }

    /// Prepare a splice, copying data from one file descriptor to another.
    #[inline]
    pub unsafe fn prep_splice(
        &mut self,
        fd_in: RawFd,
        off_in: i64,
        fd_out: RawFd,
        off_out: i64,
        count: u32,
        flags: SpliceFlags,
    ) {
        uring_sys::io_uring_prep_splice(
            self.sqe,
            fd_in,
            off_in,
            fd_out,
            off_out,
            count,
            flags.bits(),
        );
    }

    /// Prepare a `recv` event on a file descriptor.
    #[inline]
    pub unsafe fn prep_recv(&mut self, fd: impl UringFd, buf: &mut [u8], flags: MsgFlags) {
        let data = buf.as_mut_ptr() as *mut libc::c_void;
        let len = buf.len();
        uring_sys::io_uring_prep_recv(self.sqe, fd.as_raw_fd(), data, len, flags.bits());
        fd.update_sqe(self);
    }

    /// Prepare a send event on a file descriptor.
    #[inline]
    pub unsafe fn prep_send(&mut self, fd: impl UringFd, buf: &[u8], flags: MsgFlags) {
        let data = buf.as_ptr() as *const libc::c_void as *mut libc::c_void;
        let len = buf.len();
        uring_sys::io_uring_prep_send(self.sqe, fd.as_raw_fd(), data, len, flags.bits());
        fd.update_sqe(self);
    }

    /// Prepare a `recvmsg` event on a file descriptor.
    pub unsafe fn prep_recvmsg(
        &mut self,
        fd: impl UringFd,
        msg: *mut libc::msghdr,
        flags: MsgFlags,
    ) {
        uring_sys::io_uring_prep_recvmsg(self.sqe, fd.as_raw_fd(), msg, flags.bits() as _);
        fd.update_sqe(self);
    }

    /// Prepare a `sendmsg` event on a file descriptor.
    pub unsafe fn prep_sendmsg(
        &mut self,
        fd: impl UringFd,
        msg: *mut libc::msghdr,
        flags: MsgFlags,
    ) {
        uring_sys::io_uring_prep_sendmsg(self.sqe, fd.as_raw_fd(), msg, flags.bits() as _);
        fd.update_sqe(self);
    }

    /// Prepare a `fallocate` event.
    #[inline]
    pub unsafe fn prep_fallocate(
        &mut self,
        fd: impl UringFd,
        offset: u64,
        size: u64,
        flags: FallocateFlags,
    ) {
        uring_sys::io_uring_prep_fallocate(
            self.sqe,
            fd.as_raw_fd(),
            flags.bits() as _,
            offset as _,
            size as _,
        );
        fd.update_sqe(self);
    }

    /// Prepare a `statx` event.
    #[inline]
    pub unsafe fn prep_statx(
        &mut self,
        dirfd: impl UringFd,
        path: &CStr,
        flags: StatxFlags,
        mask: StatxMode,
        buf: &mut Statx,
    ) {
        uring_sys::io_uring_prep_statx(
            self.sqe,
            dirfd.as_raw_fd(),
            path.as_ptr() as _,
            flags.bits() as _,
            mask.bits() as _,
            buf as _,
        );
    }

    /// Prepare an `openat` event.
    #[inline]
    pub unsafe fn prep_openat(&mut self, fd: impl UringFd, path: &CStr, flags: OFlag, mode: Mode) {
        uring_sys::io_uring_prep_openat(
            self.sqe,
            fd.as_raw_fd(),
            path.as_ptr() as _,
            flags.bits(),
            mode.bits(),
        );
    }

    // TODO openat2

    /// Prepare a close event on a file descriptor.
    #[inline]
    pub unsafe fn prep_close(&mut self, fd: impl UringFd) {
        uring_sys::io_uring_prep_close(self.sqe, fd.as_raw_fd());
    }

    /// Prepare a timeout event.
    #[inline]
    pub unsafe fn prep_timeout(
        &mut self,
        ts: &uring_sys::__kernel_timespec,
        events: u32,
        flags: TimeoutFlags,
    ) {
        uring_sys::io_uring_prep_timeout(
            self.sqe,
            ts as *const _ as *mut _,
            events as _,
            flags.bits() as _,
        );
    }

    #[inline]
    pub unsafe fn prep_timeout_remove(&mut self, user_data: u64) {
        uring_sys::io_uring_prep_timeout_remove(self.sqe, user_data as _, 0);
    }

    #[inline]
    pub unsafe fn prep_link_timeout(&mut self, ts: &uring_sys::__kernel_timespec) {
        uring_sys::io_uring_prep_link_timeout(self.sqe, ts as *const _ as *mut _, 0);
    }

    #[inline]
    pub unsafe fn prep_poll_add(&mut self, fd: impl UringFd, poll_flags: PollFlags) {
        uring_sys::io_uring_prep_poll_add(self.sqe, fd.as_raw_fd(), poll_flags.bits());
        fd.update_sqe(self);
    }

    #[inline]
    pub unsafe fn prep_poll_remove(&mut self, user_data: u64) {
        uring_sys::io_uring_prep_poll_remove(self.sqe, user_data as _)
    }

    #[inline]
    pub unsafe fn prep_connect(&mut self, fd: impl UringFd, socket_addr: &SockaddrStorage) {
        let addr = socket_addr.as_ptr();
        let len = socket_addr.len();
        uring_sys::io_uring_prep_connect(self.sqe, fd.as_raw_fd(), addr as *mut _, len);
        fd.update_sqe(self);
    }

    #[inline]
    pub unsafe fn prep_accept(
        &mut self,
        fd: impl UringFd,
        accept: Option<&mut SockAddrStorage>,
        flags: SockFlag,
    ) {
        let (addr, len) = match accept {
            Some(accept) => (
                accept.storage.as_mut_ptr() as *mut _,
                &mut accept.len as *mut _ as *mut _,
            ),
            None => (std::ptr::null_mut(), std::ptr::null_mut()),
        };
        uring_sys::io_uring_prep_accept(self.sqe, fd.as_raw_fd(), addr, len, flags.bits());
        fd.update_sqe(self);
    }

    #[inline]
    pub unsafe fn prep_fadvise(
        &mut self,
        fd: impl UringFd,
        off: u64,
        len: u64,
        advice: PosixFadviseAdvice,
    ) {
        use PosixFadviseAdvice::*;
        let advice = match advice {
            POSIX_FADV_NORMAL => libc::POSIX_FADV_NORMAL,
            POSIX_FADV_SEQUENTIAL => libc::POSIX_FADV_SEQUENTIAL,
            POSIX_FADV_RANDOM => libc::POSIX_FADV_RANDOM,
            POSIX_FADV_NOREUSE => libc::POSIX_FADV_NOREUSE,
            POSIX_FADV_WILLNEED => libc::POSIX_FADV_WILLNEED,
            POSIX_FADV_DONTNEED => libc::POSIX_FADV_DONTNEED,
            _ => unreachable!(),
        };
        uring_sys::io_uring_prep_fadvise(self.sqe, fd.as_raw_fd(), off as _, len as _, advice);
        fd.update_sqe(self);
    }

    #[inline]
    pub unsafe fn prep_madvise(&mut self, data: &mut [u8], advice: MmapAdvise) {
        use MmapAdvise::*;
        let advice = match advice {
            MADV_NORMAL => libc::MADV_NORMAL,
            MADV_RANDOM => libc::MADV_RANDOM,
            MADV_SEQUENTIAL => libc::MADV_SEQUENTIAL,
            MADV_WILLNEED => libc::MADV_WILLNEED,
            MADV_DONTNEED => libc::MADV_DONTNEED,
            MADV_REMOVE => libc::MADV_REMOVE,
            MADV_DONTFORK => libc::MADV_DONTFORK,
            MADV_DOFORK => libc::MADV_DOFORK,
            MADV_HWPOISON => libc::MADV_HWPOISON,
            MADV_MERGEABLE => libc::MADV_MERGEABLE,
            MADV_UNMERGEABLE => libc::MADV_UNMERGEABLE,
            MADV_SOFT_OFFLINE => libc::MADV_SOFT_OFFLINE,
            MADV_HUGEPAGE => libc::MADV_HUGEPAGE,
            MADV_NOHUGEPAGE => libc::MADV_NOHUGEPAGE,
            MADV_DONTDUMP => libc::MADV_DONTDUMP,
            MADV_DODUMP => libc::MADV_DODUMP,
            MADV_FREE => libc::MADV_FREE,
            _ => unreachable!(),
        };
        uring_sys::io_uring_prep_madvise(
            self.sqe,
            data.as_mut_ptr() as *mut _,
            data.len() as _,
            advice,
        );
    }

    #[inline]
    pub unsafe fn prep_epoll_ctl(
        &mut self,
        epoll_fd: RawFd,
        op: EpollOp,
        fd: RawFd,
        event: Option<&mut EpollEvent>,
    ) {
        let op = match op {
            EpollOp::EpollCtlAdd => libc::EPOLL_CTL_ADD,
            EpollOp::EpollCtlDel => libc::EPOLL_CTL_DEL,
            EpollOp::EpollCtlMod => libc::EPOLL_CTL_MOD,
            _ => unreachable!(),
        };
        let event = event.map_or(ptr::null_mut(), |event| event as *mut EpollEvent as *mut _);
        uring_sys::io_uring_prep_epoll_ctl(self.sqe, epoll_fd, fd, op, event);
    }

    #[inline]
    pub unsafe fn prep_files_update(&mut self, files: &[RawFd], offset: u32) {
        let addr = files.as_ptr() as *mut RawFd;
        let len = files.len() as u32;
        uring_sys::io_uring_prep_files_update(self.sqe, addr, len, offset as _);
    }

    pub unsafe fn prep_provide_buffers(
        &mut self,
        buffers: &mut [u8],
        count: u32,
        group: BufferGroupId,
        index: u32,
    ) {
        let addr = buffers.as_mut_ptr() as *mut libc::c_void;
        let len = buffers.len() as u32 / count;
        uring_sys::io_uring_prep_provide_buffers(
            self.sqe,
            addr,
            len as _,
            count as _,
            group.id as _,
            index as _,
        );
    }

    pub unsafe fn prep_remove_buffers(&mut self, count: u32, id: BufferGroupId) {
        uring_sys::io_uring_prep_remove_buffers(self.sqe, count as _, id.id as _);
    }

    #[inline]
    pub unsafe fn prep_cancel(&mut self, user_data: u64, flags: i32) {
        uring_sys::io_uring_prep_cancel(self.sqe, user_data as _, flags);
    }

    /// Prepare a no-op event.
    #[inline]
    pub unsafe fn prep_nop(&mut self) {
        uring_sys::io_uring_prep_nop(self.sqe);
    }

    /// Clear event. Clears user data, flags, and any event setup.
    pub fn clear(&mut self) {
        *self.sqe = unsafe { mem::zeroed() };
    }

    /// Get a reference to the underlying
    /// [`uring_sys::io_uring_sqe`](uring_sys::io_uring_sqe) object.
    ///
    /// You can use this method to inspect the low-level details of an event.
    pub fn raw(&self) -> &uring_sys::io_uring_sqe {
        self.sqe
    }

    pub unsafe fn raw_mut(&mut self) -> &mut uring_sys::io_uring_sqe {
        self.sqe
    }
}

unsafe impl<'a> Send for SQE<'a> {}
unsafe impl<'a> Sync for SQE<'a> {}

#[derive(Debug)]
pub struct SockAddrStorage {
    storage: mem::MaybeUninit<nix::sys::socket::sockaddr_storage>,
    len: usize,
}

impl SockAddrStorage {
    pub fn uninit() -> Self {
        let storage = mem::MaybeUninit::uninit();
        let len = mem::size_of::<nix::sys::socket::sockaddr_storage>();
        SockAddrStorage { storage, len }
    }

    // pub unsafe fn as_socket_addr(&self) -> io::Result<SockAddr> {
    //     let storage = &*self.storage.as_ptr();
    //     nix::sys::socket::sockaddr_storage_to_addr(storage, self.len).map_err(|e| to_io_error!(e))
    // }
}

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct BufferGroupId {
    pub id: u32,
}

bitflags::bitflags! {
    /// [`SQE`](SQE) configuration flags.
    #[derive(Debug, Clone, Copy)]
    pub struct SubmissionFlags: u8 {
        /// This event's file descriptor is an index into the preregistered set of files.
        const FIXED_FILE    = 1 << 0;   /* use fixed fileset */
        /// Submit this event only after completing all ongoing submission events.
        const IO_DRAIN      = 1 << 1;   /* issue after inflight IO */
        /// Force the next submission event to wait until this event has completed successfully.
        ///
        /// An event's link only applies to the next event, but link chains can be
        /// arbitrarily long.
        const IO_LINK       = 1 << 2;   /* next IO depends on this one */

        const IO_HARDLINK   = 1 << 3;
        const ASYNC         = 1 << 4;
        const BUFFER_SELECT = 1 << 5;
    }
}

bitflags::bitflags! {
    pub struct FsyncFlags: u32 {
        /// Sync file data without an immediate metadata sync.
        const FSYNC_DATASYNC    = 1 << 0;
    }
}

bitflags::bitflags! {
    pub struct StatxFlags: i32 {
        const AT_STATX_SYNC_AS_STAT = 0;
        const AT_SYMLINK_NOFOLLOW   = 1 << 10;
        const AT_NO_AUTOMOUNT       = 1 << 11;
        const AT_EMPTY_PATH         = 1 << 12;
        const AT_STATX_FORCE_SYNC   = 1 << 13;
        const AT_STATX_DONT_SYNC    = 1 << 14;
    }
}

bitflags::bitflags! {
    pub struct StatxMode: i32 {
        const STATX_TYPE        = 1 << 0;
        const STATX_MODE        = 1 << 1;
        const STATX_NLINK       = 1 << 2;
        const STATX_UID         = 1 << 3;
        const STATX_GID         = 1 << 4;
        const STATX_ATIME       = 1 << 5;
        const STATX_MTIME       = 1 << 6;
        const STATX_CTIME       = 1 << 7;
        const STATX_INO         = 1 << 8;
        const STATX_SIZE        = 1 << 9;
        const STATX_BLOCKS      = 1 << 10;
        const STATX_BTIME       = 1 << 11;
    }
}

bitflags::bitflags! {
    pub struct TimeoutFlags: u32 {
        const TIMEOUT_ABS   = 1 << 0;
    }
}

bitflags::bitflags! {
    pub struct SpliceFlags: u32 {
        const F_FD_IN_FIXED = 1 << 31;
    }
}

/// A sequence of [`SQE`]s from the [`SubmissionQueue`][crate::SubmissionQueue].
pub struct SQEs<'ring> {
    sq: &'ring mut uring_sys::io_uring,
    count: u32,
    consumed: u32,
}

impl<'ring> SQEs<'ring> {
    pub(crate) fn new(sq: &'ring mut uring_sys::io_uring, count: u32) -> SQEs<'ring> {
        SQEs {
            sq,
            count,
            consumed: 0,
        }
    }

    /// An iterator of [`HardLinkedSQE`]s. These will be [`SQE`]s that are
    /// *hard-linked* together.
    ///
    /// Hard-linked SQEs will occur sequentially. All of them will be completed,
    /// even if one of the events resolves to an error.
    pub fn hard_linked(&mut self) -> HardLinked<'ring, '_> {
        HardLinked { sqes: self }
    }

    /// An iterator of [`SoftLinkedSQE`]s. These will be [`SQE`]s that are
    /// *soft-linked* together.
    ///
    /// Soft-linked SQEs will occur sequentially. If one the events errors, all
    /// events after it will be cancelled.
    pub fn soft_linked(&mut self) -> SoftLinked<'ring, '_> {
        SoftLinked { sqes: self }
    }

    /// Remaining [`SQE`]s that can be modified.
    pub fn remaining(&self) -> u32 {
        self.count - self.consumed
    }

    fn consume(&mut self) -> Option<SQE<'ring>> {
        if self.consumed < self.count {
            unsafe {
                let sqe = uring_sys::io_uring_get_sqe(self.sq);
                uring_sys::io_uring_prep_nop(sqe);
                self.consumed += 1;
                Some(SQE { sqe: &mut *sqe })
            }
        } else {
            None
        }
    }
}

impl<'ring> Iterator for SQEs<'ring> {
    type Item = SQE<'ring>;

    fn next(&mut self) -> Option<SQE<'ring>> {
        self.consume()
    }
}

/// An Iterator of [`SQE`]s which will be hard linked together.
pub struct HardLinked<'ring, 'a> {
    sqes: &'a mut SQEs<'ring>,
}

impl<'ring> HardLinked<'ring, '_> {
    pub fn terminate(self) -> Option<SQE<'ring>> {
        self.sqes.consume()
    }
}

impl<'ring> Iterator for HardLinked<'ring, '_> {
    type Item = HardLinkedSQE<'ring>;

    fn next(&mut self) -> Option<Self::Item> {
        let is_final = self.sqes.remaining() == 1;
        self.sqes
            .consume()
            .map(|sqe| HardLinkedSQE { sqe, is_final })
    }
}

pub struct HardLinkedSQE<'ring> {
    sqe: SQE<'ring>,
    is_final: bool,
}

impl<'ring> Deref for HardLinkedSQE<'ring> {
    type Target = SQE<'ring>;

    fn deref(&self) -> &SQE<'ring> {
        &self.sqe
    }
}

impl<'ring> DerefMut for HardLinkedSQE<'ring> {
    fn deref_mut(&mut self) -> &mut SQE<'ring> {
        &mut self.sqe
    }
}

impl<'ring> Drop for HardLinkedSQE<'ring> {
    fn drop(&mut self) {
        if !self.is_final {
            self.sqe.set_flags(SubmissionFlags::IO_HARDLINK);
        }
    }
}

/// An Iterator of [`SQE`]s which will be soft linked together.
pub struct SoftLinked<'ring, 'a> {
    sqes: &'a mut SQEs<'ring>,
}

impl<'ring> SoftLinked<'ring, '_> {
    pub fn terminate(self) -> Option<SQE<'ring>> {
        self.sqes.consume()
    }
}

impl<'ring> Iterator for SoftLinked<'ring, '_> {
    type Item = SoftLinkedSQE<'ring>;

    fn next(&mut self) -> Option<Self::Item> {
        let is_final = self.sqes.remaining() == 1;
        self.sqes
            .consume()
            .map(|sqe| SoftLinkedSQE { sqe, is_final })
    }
}

pub struct SoftLinkedSQE<'ring> {
    sqe: SQE<'ring>,
    is_final: bool,
}

impl<'ring> Deref for SoftLinkedSQE<'ring> {
    type Target = SQE<'ring>;

    fn deref(&self) -> &SQE<'ring> {
        &self.sqe
    }
}

impl<'ring> DerefMut for SoftLinkedSQE<'ring> {
    fn deref_mut(&mut self) -> &mut SQE<'ring> {
        &mut self.sqe
    }
}

impl<'ring> Drop for SoftLinkedSQE<'ring> {
    fn drop(&mut self) {
        if !self.is_final {
            self.sqe.set_flags(SubmissionFlags::IO_LINK);
        }
    }
}
