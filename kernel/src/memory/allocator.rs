use crate::kernel;
use core::{
    alloc::{GlobalAlloc, Layout},
    ptr,
};

use crate::{
    memory::{
        align_up,
        paging::{EntryFlags, IterPage, Page, PAGE_SIZE},
    },
    utils::Locked,
};

use super::paging::current_root_table;

#[derive(Debug)]
pub struct Node {
    size: usize,
    next: Option<&'static mut Node>,
}

impl Node {
    pub const fn new(size: usize) -> Self {
        Self { size, next: None }
    }

    pub fn start_addr(&self) -> usize {
        self as *const Self as usize
    }

    pub fn end_addr(&self) -> usize {
        self.start_addr() + self.size
    }

    /// checks if a node can hold `size` bytes aligned to `align_amount`
    pub fn can_hold(&self, size: usize, align_amount: usize) -> Result<usize, ()> {
        let start = align_up(self.start_addr(), align_amount);
        let end = start.checked_add(size).ok_or(())?;

        if end > self.end_addr() {
            return Err(());
        }

        let ecess_size = self.end_addr() - end;
        if ecess_size > 0 && ecess_size < size_of::<Node>() {
            // if we have an excess we check if we can use it for a new node or not if not Err
            return Err(());
        }

        Ok(start)
    }
}
#[derive(Debug)]
pub struct LinkedListAllocator {
    head: Node,
    /// keeps track of the current heap_end so we can extend it later
    pub heap_end: usize,
}

impl LinkedListAllocator {
    pub const fn new() -> Self {
        Self {
            head: Node {
                size: 0,
                next: None,
            },

            heap_end: 0,
        }
    }

    /// size may not be equal to `size`, heap_start may not be equal to `possible_start` these are
    /// just boundaries
    /// unsafe because possible_start has to be mapped first
    pub unsafe fn init(&mut self, possible_start: usize, size: usize) {
        let heap_start = align_up(possible_start, size_of::<Node>());
        let size = size - (heap_start - possible_start);

        let heap_end = heap_start + size;
        self.heap_end = heap_end;

        self.add_free_node(heap_start, size);
    }

    pub unsafe fn alloc_mut(&mut self, layout: Layout) -> *mut u8 {
        let (size, align) = Self::size_align(layout);

        if let Some((node, addr)) = self.find_free_node(size, align) {
            let alloc_end = addr.checked_add(size).expect("overflow");
            // divide block
            let excess_size = node.end_addr() - alloc_end;
            if excess_size > 0 {
                self.add_free_node(alloc_end, excess_size);
            }

            addr as *mut u8
        } else {
            ptr::null_mut()
        }
    }

    pub unsafe fn dealloc_mut(&mut self, ptr: *mut u8, layout: Layout) {
        let (size, _) = Self::size_align(layout);
        self.add_free_node(ptr as usize, size)
    }

    pub fn find_free_node(
        &mut self,
        size: usize,
        align: usize,
    ) -> Option<(&'static mut Node, usize)> {
        let mut current = &mut self.head;

        while let Some(ref mut node) = current.next {
            if let Ok(addr) = node.can_hold(size, align) {
                let next = node.next.take();
                let node = current.next.take().unwrap();

                current.next = next;

                return Some((node, addr));
            } else {
                current = current.next.as_mut().unwrap();
            }
        }

        //  TODO: add an extend_by function to extend the heap by size
        //  TODO: add a heap_max that prevents heap from extending further
        self.extend_heap().ok()?;
        self.find_free_node(size, align)
    }

    pub unsafe fn add_free_node(&mut self, addr: usize, size: usize) {
        assert_eq!(align_up(addr, align_of::<Node>()), addr);
        assert!(size >= size_of::<Node>());

        let mut node = Node::new(size);

        node.next = self.head.next.take();

        let node_ptr = addr as *mut Node;
        ptr::write_volatile(node_ptr, node);
        self.head.next = Some(&mut *node_ptr);
    }

    pub const PAGES_PER_EXTEND: usize = 128;
    /// extends the heap by `PAGES_PER_EXTEND` pages
    pub fn extend_heap(&mut self) -> Result<(), ()> {
        let start_page = Page::containing_address(self.heap_end + PAGE_SIZE);
        let end_page = Page::containing_address(self.heap_end + PAGE_SIZE * Self::PAGES_PER_EXTEND);
        let iter = IterPage {
            start: start_page,
            end: end_page,
        };

        for page in iter {
            unsafe {
                let allocated_frame = kernel().frame_allocator().allocate_frame().ok_or(())?;

                current_root_table()
                    .map_to(
                        page,
                        allocated_frame,
                        EntryFlags::PRESENT | EntryFlags::WRITABLE,
                    )
                    .or(Err(()))?;
            }
        }
        unsafe {
            self.add_free_node(start_page.start_address, PAGE_SIZE * Self::PAGES_PER_EXTEND);
        }
        // self.head.next should contain our extended Node we combine all the extended Nodes
        // togther
        while let Some(ref mut node) = self.head.next.as_mut().unwrap().next {
            if !(node.size % (PAGE_SIZE * Self::PAGES_PER_EXTEND) == 0) {
                break;
            }

            let node_next = node.next.take();
            let node_size = node.size;

            let to_combine = self.head.next.take().unwrap();
            to_combine.next = node_next;

            to_combine.size = to_combine.size + node_size;

            self.head.next = Some(to_combine);
        }

        self.heap_end = end_page.start_address + PAGE_SIZE;
        Ok(())
    }

    fn size_align(layout: Layout) -> (usize, usize) {
        let layout = layout
            .align_to(align_of::<Node>())
            .expect("adjusting alignment failed")
            .pad_to_align();

        let size = layout.size().max(size_of::<Node>());
        (size, layout.align())
    }
}

unsafe impl GlobalAlloc for Locked<LinkedListAllocator> {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut allocator = self.inner.lock();
        allocator.alloc_mut(layout)
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let mut allocator = self.inner.lock();
        allocator.dealloc_mut(ptr, layout)
    }
}
