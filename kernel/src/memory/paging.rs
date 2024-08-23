const ENTRY_COUNT: usize = 512;
const HIGHER_HALF_ENTRY: usize = 256;

pub const PAGE_SIZE: usize = 4096;
use crate::{
    kernel,
    memory::{translate, PhysAddr},
};
use bitflags::bitflags;
use core::{
    arch::asm,
    ops::{Index, IndexMut},
};

use crate::memory::frame_allocator::Frame;

use super::{align_down, frame_allocator::RegionAllocator, VirtAddr};

#[derive(Debug, Clone, Copy)]
pub struct Page {
    pub start_address: VirtAddr,
}
#[derive(Debug)]
pub struct IterPage {
    pub start: Page,
    pub end: Page,
}

impl Page {
    pub const fn containing_address(address: VirtAddr) -> Self {
        Self {
            start_address: align_down(address, PAGE_SIZE),
        }
    }

    pub const fn iter_pages(start: Page, end: Page) -> IterPage {
        IterPage { start, end }
    }
}

impl Iterator for IterPage {
    type Item = Page;
    fn next(&mut self) -> Option<Self::Item> {
        if self.start.start_address <= self.end.start_address {
            let page = self.start;

            let max_page_addr = usize::MAX - (PAGE_SIZE - 1);
            if self.start.start_address < max_page_addr {
                self.start.start_address += PAGE_SIZE;
            } else {
                self.end.start_address -= PAGE_SIZE;
            }
            Some(page)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone)]
pub struct Entry(PhysAddr);
// address of the next table or physial frame in 0x000FFFFF_FFFFF000 (the fs is the address are the fs the rest are flags or reserved)

#[cfg(target_arch = "x86_64")]
impl Entry {
    pub fn frame(&self) -> Option<Frame> {
        if self.flags().contains(EntryFlags::PRESENT) {
            // TODO: figure out more info about the max physical address width
            return Some(Frame::containing_address(self.0 & 0x000FFFFF_FFFFF000));
        }
        None
    }

    pub fn flags(&self) -> EntryFlags {
        EntryFlags::from_bits_truncate(self.0 as u64)
    }

    pub const fn new(flags: EntryFlags, addr: PhysAddr) -> Self {
        Self(addr | flags.bits() as usize)
    }

    pub const fn set(&mut self, flags: EntryFlags, addr: PhysAddr) {
        *self = Self::new(flags, addr)
    }

    /// deallocates an entry depending on it's level if it is 1 it should just deallocate the frame
    /// otherwise treat the frame as a page table and deallocate it
    /// &mut self becomes invaild after
    pub unsafe fn free(&mut self, level: u8) {
        let frame = self.frame().unwrap();

        if level == 0 {
            kernel().frame_allocator().deallocate_frame(frame);
        }
        let table = &mut *((frame.start_address + kernel().phy_offset) as *mut PageTable);
        table.free(level)
    }
}

#[cfg(target_arch = "x86_64")]
bitflags! {
    #[derive(Debug, Clone, Copy)]
    pub struct EntryFlags: u64 {
        const PRESENT =         1;
        const WRITABLE =        1 << 1;
        const USER_ACCESSIBLE = 1 << 2;
        const WRITE_THROUGH =   1 << 3;
        const NO_CACHE =        1 << 4;
        const ACCESSED =        1 << 5;
        const DIRTY =           1 << 6;
        const HUGE_PAGE =       1 << 7;
        const GLOBAL =          1 << 8;
        const NO_EXECUTE =      1 << 63;
    }
}

#[derive(Debug, Clone)]
pub struct PageTable {
    pub entries: [Entry; ENTRY_COUNT],
}

impl PageTable {
    pub fn zeroize(&mut self) {
        for entry in &mut self.entries {
            entry.0 = 0;
        }
    }

    /// copies the higher half entries of the current pml4 to this page table
    pub fn copy_higher_half(&mut self) {
        unsafe {
            self.entries[HIGHER_HALF_ENTRY..ENTRY_COUNT]
                .clone_from_slice(&current_root_table().entries[HIGHER_HALF_ENTRY..ENTRY_COUNT])
        }
    }
    /// deallocates a page table including it's entries, doesn't deallocate the higher half!
    /// unsafe because self becomes invaild after
    pub unsafe fn free(&mut self, level: u8) {
        for entry in &mut self.entries[0..HIGHER_HALF_ENTRY] {
            if entry.0 != 0 {
                entry.free(level - 1);
            }
        }

        let table_addr = self as *mut PageTable as VirtAddr;

        let frame = Frame::containing_address(table_addr - kernel().phy_offset);
        kernel().frame_allocator().deallocate_frame(frame)
    }
}

impl Index<usize> for PageTable {
    type Output = Entry;
    fn index(&self, index: usize) -> &Self::Output {
        &self.entries[index]
    }
}

impl IndexMut<usize> for PageTable {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.entries[index]
    }
}

/// returns the current pml4 from cr3
#[cfg(target_arch = "x86_64")]
pub unsafe fn current_root_table() -> &'static mut PageTable {
    use crate::kernel;

    let phys_addr: PhysAddr;
    unsafe {
        asm!("mov {}, cr3", out(reg) phys_addr);
    }
    let frame = Frame::containing_address(phys_addr);

    let virt_addr = frame.start_address + kernel().phy_offset;

    &mut *(virt_addr as *mut PageTable)
}

#[derive(Debug)]
pub enum MapToError {
    FrameAllocationFailed,
}

impl Entry {
    /// changes the entry flags to `flags`
    /// if the entry is not present it allocates a new frame and uses it's address as entry's
    /// then returns the entry address as a pagetable
    #[cfg(target_arch = "x86_64")]
    fn map(
        &mut self,
        flags: EntryFlags,
        frame_allocator: &mut RegionAllocator,
    ) -> Result<&'static mut PageTable, MapToError> {
        use crate::kernel;

        if self.flags().contains(EntryFlags::PRESENT) {
            let addr = self.frame().unwrap().start_address;

            self.set(flags | self.flags(), addr);
            let virt_addr = addr + kernel().phy_offset;
            let entry_ptr = virt_addr as *mut PageTable;

            Ok(unsafe { &mut *(entry_ptr) })
        } else {
            let frame = frame_allocator
                .allocate_frame()
                .ok_or(MapToError::FrameAllocationFailed)?;

            let addr = frame.start_address;
            self.set(flags, addr);

            let virt_addr = addr + kernel().phy_offset;
            let table_ptr = virt_addr as *mut PageTable;

            Ok(unsafe {
                (*table_ptr).zeroize();
                &mut *(table_ptr)
            })
        }
    }
}

impl PageTable {
    /// maps a virtual `Page` to physical `Frame`
    pub fn map_to(
        &mut self,
        page: Page,
        frame: Frame,
        flags: EntryFlags,
    ) -> Result<(), MapToError> {
        let (_, level_1_index, level_2_index, level_3_index, level_4_index) =
            translate(page.start_address);
        let frame_allocator = &mut kernel().frame_allocator();
        let level_3_table = self[level_4_index].map(flags, frame_allocator)?;

        let level_2_table = level_3_table[level_3_index].map(flags, frame_allocator)?;

        let level_1_table = level_2_table[level_2_index].map(flags, frame_allocator)?;

        let entry = &mut level_1_table[level_1_index];

        *entry = Entry::new(flags, frame.start_address);
        Ok(())
    }

    /// maps a kernel only `Page` to `Frame` with present and writeable flags
    pub fn map_to_writeable(&mut self, page: Page, frame: Frame) -> Result<(), MapToError> {
        let flags = EntryFlags::PRESENT | EntryFlags::WRITABLE;
        self.map_to(page, frame, flags)
    }
}

pub unsafe fn flush() {
    #[cfg(target_arch = "x86_64")]
    asm!("invlpg [{}]", in(reg) 0 as *const u8);
}

/// allocates a pml4 and returns its physical address
pub fn allocate_pml4() -> Result<PhysAddr, MapToError> {
    let frame = kernel()
        .frame_allocator()
        .allocate_frame()
        .ok_or(MapToError::FrameAllocationFailed)?;

    let virt_start_addr = frame.start_address + kernel().phy_offset;
    let table = unsafe { &mut *(virt_start_addr as *mut PageTable) };

    table.zeroize();
    table.copy_higher_half();

    Ok(frame.start_address)
}
