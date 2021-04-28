use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::null_mut;
use std::mem::{size_of,transmute};
use std::convert::{TryFrom,TryInto};
use cfg_if::cfg_if;

pub mod heap;

use heap::{HostHeap,HeapType};

/// The number of host memory pages to allocate for all memory allocations. If these fill up then all future allocations will fail.
const MAX_HOST_PAGES: usize = 200;

/// MAX_HOST_PAGES as an isize.
const MAX_HOST_PAGES_ISIZE: isize = MAX_HOST_PAGES as isize;

/// Number of bytes which can be allocated from one MiniPage.
const MINI_PAGE_ALLOCABLE_BYTES: u16 = 2048;

const FRESH_REUSED_RATIO: f64 = 1_f64;

/// Size of the MiniPageHeader.free_segments array
const MINI_PAGE_FREE_SEGMENTS_SIZE: usize = 257;

/// Size of a mini page with the header.
const MINI_PAGE_TOTAL_BYTES: isize = (MINI_PAGE_ALLOCABLE_BYTES as isize) + (size_of::<MiniPageHeader>() as isize);
/// MINI_PAGE_TOTAL_BYTES as an usize
const MINI_PAGE_TOTAL_BYTES_USIZE: usize = MINI_PAGE_TOTAL_BYTES as usize;
/// MINI_PAGE_TOTAL_BYTES as an u32
const MINI_PAGE_TOTAL_BYTES_U32: u32 = MINI_PAGE_TOTAL_BYTES as u32;
/// MINI_PAGE_TOTAL_BYTES as an f64
const MINI_PAGE_TOTAL_BYTES_F64: f64 = MINI_PAGE_TOTAL_BYTES as f64;

/// The largest size class we can allocate right now.
/// Multi-page allocations are not supported yet.
const MAX_SIZE_CLASS: u8 = 11;

/// MAX_SIZE_CLASS but a usize
const MAX_SIZE_CLASS_USIZE: usize = 11;

cfg_if! {
    if #[cfg(feature = "metrics")] {
        /// Records metrics about the allocation process.
        /// heap_bytes_{read,write} record memory operations. It does not record accesses to AllocatorImpl but does record any memory operations in MetaPage, UnsafeStack, and MiniPageHeader. 
        #[derive(Copy, Clone, Debug)]
        pub struct AllocMetrics {
            /// Total number of allocations for each size class.
            pub total_allocs: [u32; MAX_SIZE_CLASS_USIZE + 1],

            /// Total number of deallocations for each size class.
            pub total_deallocs: [u32; MAX_SIZE_CLASS_USIZE + 1],

            /// Total number of MiniPages used.
            pub total_minipages: u32,
            
            /// Cumulative memory read operations. Unit of bytes.
            pub heap_bytes_read: usize,

            /// Cumulative memory write operations. Unit of bytes.
            pub heap_bytes_write: usize,
        }

        impl AllocMetrics {
            /// Allocate an AllocMetrics at start_addr. Returns a pointer to the allocation and a pointer to the byte of memory after the allocation.
            unsafe fn alloc(start_addr: *mut u8) -> (*mut AllocMetrics, *mut u8) {
                // Allocate
                let metrics_ptr = start_addr as *mut AllocMetrics;
                (*metrics_ptr).total_allocs = [0; MAX_SIZE_CLASS_USIZE + 1];
                (*metrics_ptr).total_deallocs = [0; MAX_SIZE_CLASS_USIZE + 1];
                (*metrics_ptr).total_minipages = 0;
                (*metrics_ptr).heap_bytes_read = 0;
                (*metrics_ptr).heap_bytes_write = 0;

                // Determine byte of memory after the allocation
                let next_ptr = metrics_ptr.offset(1) as *mut u8;

                return (metrics_ptr, next_ptr);
            }
        }
    }
}

/// Allocates an initial number of memory pages, then
/// maintains a free linked list.
struct AllocatorImpl<H> where H: HostHeap {
    /// True if the initial call to allocate all the
    /// memory we will use has been made.
    /// next_minipage_addr is only
    /// guaranteed to not be null when did_init_heap
    /// is true.
    did_init_heap: bool,
    
    /// The HostHeap implementation for the
    /// current platform.
    heap: UnsafeCell<H>,

    /// Head of MiniPage header free list for each size class.
    free_lists: [*mut MiniPageHeader; MAX_SIZE_CLASS_USIZE + 1],

    /// The first MiniPage worth of space in the heap is reserved for this "meta page". It is used to store information which needs to be placed on the heap for the Allicator implementation. Some if allocated and None if not allocated yet.
    meta_page: Option<*mut MetaPage>,

    /// The address of the first byte of memory which can be used to satisfy user allocation requests. Takes into account the space for the meta-page. Only non-null after ensure_meta_page() is called.
    alloc_start_ptr: *mut u8,

    /// Next address which can be used for a new MiniPage.
    next_minipage_addr: *mut u8,

    /// Total number of allocations for each size class which were performed from a reused MiniPage header.
    total_alloc_reused: [u32; MAX_SIZE_CLASS_USIZE + 1],

    /// Total number of allocations for each size class which were performed from a newly allocated MiniPage header.
    total_alloc_fresh: [u32; MAX_SIZE_CLASS_USIZE + 1],

    /// Address of the current fresh MiniPage for each size class. null_mut() if there is not one.
    fresh_minipages: [*mut MiniPageHeader; MAX_SIZE_CLASS_USIZE + 1],

    /// Cause of the failure.
    #[cfg(feature = "metrics")]
    failure: Option<AllocFail>,
}

cfg_if! {
    if #[cfg(feature = "metrics")] {
        /// Indicates what type of fatal error occured while allocating.
        #[derive(Debug, Copy, Clone)]
        pub enum AllocFail {
            /// The allocation requested 0 bytes, which is not allowed.
            NoZeroAlloc,

            /// We attempted to grow the host heap, from which we hand out allocations, and failed. We cannot recover from this. 
            HostGrowFail,

            /// Big allocations are not supported yet, and the allocation requested a size that requires big allocations.
            BigAllocTODO,

            /// Failed to add a new MiniPage because there is no room left of the heap.
            AddMiniPageNoSpace,

            /// A MiniPage taken off a free minipages stack ended up not having free segments. This is a breach of the free minipages stack "contract", where all MiniPages on this stack should have at least one free segment.
            FreeMiniPagesContractBreach,
        }
    }
}

/// The first MiniPage of the heap will hold some metadata which we don't want / can't put in the AllocatorImpl stack object.
struct MetaPage {
    /// Indexes of free MiniPages for each size class. The head of each list is the currently used MiniPage for that size class. The free_segments stack will track free indexes for this MiniPage. MiniPages are popped off these stacks when their free_segments stack is empty (aka when there are no free segments on the MiniPage).
    free_minipages: [*mut UnsafeStack<*mut MiniPageHeader>; MAX_SIZE_CLASS_USIZE + 1], // + 1 bc MAX_SIZE_CLASS starts counting at 0

    /// Free segment indexes from the head of free_minipages for each size class. Allows us to avoid searching the MiniPageHeader bitmap for the most recently used MiniPage.
    free_segments: [*mut UnsafeStack<u16>; MAX_SIZE_CLASS_USIZE + 1],

    /// Allocator metrics
    #[cfg(feature = "metrics")]
    metrics: *mut AllocMetrics,
}

impl MetaPage {
    /// Allocates a MetaPage at the specified alloc_ptr. Returns the tuple: (metapage ptr, next ptr). Where next ptr is the next byte of memory after the allocated MetaPage.
    unsafe fn alloc(alloc_ptr: *mut u8) -> (*mut MetaPage, *mut u8) {
        let page_ptr = alloc_ptr as *mut MetaPage;

        // Space after this MetaPage struct in which we can place other allocations
        let mut next_ptr = page_ptr.offset(1) as *mut u8;

        // Setup free minipages stacks
        for i in 0..=MAX_SIZE_CLASS {
            let (stack, after_ptr) = UnsafeStack::<*mut MiniPageHeader>::alloc(
                next_ptr,
                (2_u32.pow(u32::from(i))).try_into().unwrap(),
            );
            (*page_ptr).free_minipages[usize::from(i)] = stack;
            next_ptr = after_ptr;
        }

        // Setup free segements stacks
        for i in 0..=MAX_SIZE_CLASS {
            let size_class = SizeClass::new(i);
            
            let (stack, after_ptr) = UnsafeStack::<u16>::alloc(
                next_ptr,
                size_class.segments_max_num(),
            );
            (*page_ptr).free_segments[usize::from(i)] = stack;
            next_ptr = after_ptr;
        }
        
        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // Setup metrics if feature is enabled
                let (metrics, after_ptr) = AllocMetrics::alloc(next_ptr);
                (*page_ptr).metrics = metrics;
                
                next_ptr = after_ptr;
            }
        }

        return (page_ptr, next_ptr);
    }
}

/// Stack stored on the heap. Implemented as a fixed size circular stack. Does not implement size growing. Can store up to 2^16 items.
#[derive(Copy, Clone, Debug)]
struct UnsafeStack<T> where T: Copy {
    /// Address of the first data index. Items in the stack will be stored in a contiguous segment following this location.
    data_ptr: *mut T,

    /// Maximum number of T items.
    max_size: u16,

    /// Current size of the stack.
    size: u16,

    /// The index of the head within the data memory segment.
    head_idx: u16,
}

impl <T> UnsafeStack<T> where T: Copy {
    /// Initialize an unsafe stack in memory at the addr.
    /// Returns the tuple: (UnsafeStack, next ptr), the next ptr is a pointer to the next byte of memory after the new UnsafeStack structure and the following data. Memory between start_addr and next ptr is managed by the new UnsafeStack.
    ///
    /// # Panics
    /// If the size of T is larger than what can be represented by isize. But the overall Alligator is the only one who should be using this structure, so this should never happen.
    unsafe fn alloc(start_addr: *mut u8, max_size: u16) -> (*mut UnsafeStack<T>, *mut u8) {
        // Setup new UnsafeStack
        let stack_ptr = start_addr as *mut UnsafeStack<T>;
        
        (*stack_ptr).data_ptr = start_addr.offset(size_of::<UnsafeStack<T>>().try_into().unwrap()) as *mut T; // TODO: Align this
        (*stack_ptr).max_size = max_size;
        (*stack_ptr).size = 0;
        (*stack_ptr).head_idx = 0;

        // Calculate next ptr
        let next_ptr = (*stack_ptr).data_ptr.offset(max_size.try_into().unwrap()) as *mut u8; // TODO: Align this

        return (stack_ptr, next_ptr);
    }

    /// Returns a MemAddr which points to the location in the heap for a data item of type T at index i.
    ///
    /// # Panics
    /// If the size of T is larger than what can be represented by isize. But the overall Alligator is the only one who should be using this structure, so this should never happen.
    unsafe fn item_ptr(&mut self, i: u16) -> *mut T {
        self.data_ptr.offset((i % self.max_size).try_into().unwrap())
    }

    /// Push an item onto the stack. Returns the Some(item) on success and None if there was no more space.
    unsafe fn push(&mut self, item: T) -> Option<T> {
        // Check there is space remaining
        if self.size == self.max_size {
            return None;
        }

        // Push
        let item_ptr = self.item_ptr(self.size);
        *item_ptr = item;

        self.size += 1;

        Some(item)
    }

    /// Pop an item from the head of the stack. Returns Some(item) on success and None if there are no items on the stack.
    unsafe fn pop(&mut self) -> Option<T> {
        // Get item 
        match self.peek() {
            None => None,
            Some(item) => {
                // Remove from stack
                self.size -= 1;
                self.head_idx = (self.head_idx + 1) % self.max_size;
        
                Some(item)
            },
        }
    }

    /// Return the item at the head of the stack without removing it. Returns None if the stack is empty.
    unsafe fn peek(&mut self) -> Option<T> {
        // Check if empty
        if self.size == 0 {
            return None;
        }

        // Get item
        let item_ptr = self.item_ptr(self.size - 1);
        let item = *item_ptr;

        Some(item)
    }

    cfg_if! {
        if #[cfg(feature = "metrics")] {
            /// Records the cost of a push operation in the meta-page for metrics. Only records the cost of accessing the self.data array as all the custodial accesses of self.size, ect are constant.
            unsafe fn record_push_cost(&mut self, meta_page: *mut MetaPage) {
                (*(*meta_page).metrics).heap_bytes_write += size_of::<T>();
            }

            /// Records the cost of a pop operation in the meta-page for metrics. Only records the cost of accessing the self.data array as all the custodial accesses of self.size, ect are constant.
            unsafe fn record_pop_cost(&mut self, meta_page: *mut MetaPage) {
                self.record_peek_cost(meta_page);
            }

            /// Records the cost of a peek operation in the meta-page for metrics. Only records the cost of accessing the self.data array as all the custodial accesses of self.size, ect are constant.
            unsafe fn record_peek_cost(&mut self, meta_page: *mut MetaPage) {
                (*(*meta_page).metrics).heap_bytes_read += size_of::<T>();
            }
        }
    }
}

/// Header for a MiniPage.
#[derive(Debug)]
struct MiniPageHeader {
    /// The next free node of the same size class.
    next: Option<*mut MiniPageHeader>,

    /// Size class exponent
    size_class_exp: u8,

    /// Bit-packed free list. A 1 means that segment is free, 0 means allocated.
    free_segments: [u8; MINI_PAGE_FREE_SEGMENTS_SIZE],

    /// True if this MiniPage is on the Allocator's free minipages stack. Storing this flag here allows us to not do a linear search through the entire free minipages stack every deallocation.
    on_free_minipages_stack: bool,
}

/// Calculates useful size class values.
#[derive(Copy, Clone)]
pub struct SizeClass {
    /// Exponent value n in 2^n which identifies size class.
    pub exp: u8,
}

impl SizeClass {
    /// New size class from an exponent number.
    pub fn new(exp: u8) -> SizeClass {
        SizeClass{
            exp: exp,
        }
    }

    /// Creates the size class required to fit a number of bytes.
    pub fn new_from_bytes(bytes: u16) -> SizeClass {
        let fbytes = f32::from(bytes);
        // # Panics
        // Won't panic because fbytes is representing a unsigned 16 bit number, and the integer log2 version of this will be 8 bits.
        let exp = fbytes.log2().ceil() as u32;
        let exp_u8 = u8::try_from(exp).unwrap();

        SizeClass{
            exp: exp_u8,
        }
    }

    /// Exponent as a usize, useful for indexing into arrays.
    pub fn exp_usize(self) -> usize {
        usize::from(self.exp)
    }
    
    /// Size of a segment in bytes.
    /// Returns u16 because the maximum size class is 11 and 2^11 requires at least 11 bits, thus 16 are required.
    pub fn segment_bytes(self) -> u16 {
        2_u16.pow(u32::from(self.exp))
    }

    /// Returns the maximum number of segments which can be stored in a MiniPage for this size class.
    pub fn segments_max_num(&self) -> u16 {
        MINI_PAGE_ALLOCABLE_BYTES / self.segment_bytes()
    }
}

/// Represents an allocated address in memory within the context of the memory allocator design.
#[derive(Copy, Clone)]
struct AllocAddr {
    /// Numerical representation of address. This is used to complete all pointer math against.
    /// The unsafe as_ptr() method will turn this number into a memory address which is gaurenteed to be safe.
    /// This address assumes the base memory address of the heap is 0. This simplifies calculations. The actual heap base offset is added via as_ptr().
    addr: u32,
}

impl AllocAddr {
    /// Initializes an AllocAddr from a u32.
    fn new(addr: u32) -> AllocAddr {
        AllocAddr{
            addr: addr,
        }
    }

    /// Initializes an AllocAddr from a usize.
    ///
    /// # Panics
    /// Shouldn't panic because:
    /// - program only supports 32 bit machines => usize will be 32 bits
    /// - usize is 32 bits => cast from usize to u32 shouldn't fail
    fn from_usize(u: usize) -> AllocAddr {
        AllocAddr{
            addr: u as u32,
        }
    }

    /// Initializes an AllocAddr from a raw pointer and heap base pointer. The returned AllocAddr will represent the raw_ptr, the base_ptr will be used to determine the start of the heap. As all AllocAddrs are relative to this address.
    /// 
    /// # Safety
    /// Calls std::mem::transmute which is only safe if the result type is the same size as the input type. For this method this is the case because:
    /// - program only supports 32 bit addresses => a raw pointer will be a 32 bit unsigned number
    /// - target type of transmute is a 32 bit unsigned integer => transmute call is safe
    unsafe fn from_ptr(base_ptr: *mut u8, raw_ptr: *mut u8) -> AllocAddr {
        let base_n = transmute::<*mut u8, u32>(base_ptr);
        let raw_n = transmute::<*mut u8, u32>(raw_ptr);
        assert!(base_n <= raw_n, "Address ({:?}) from which to make AllocAddr cannot be less than the base_ptr ({:?})", raw_ptr, base_ptr);
        
        AllocAddr{
            addr: raw_n - base_n,
        }
    }

    /// Returns information about the MiniPage from which this memory address was allocated.
    fn get_page_meta(self) -> MiniPageMeta {
        MiniPageMeta::from_addr(self)
    }

    /// Returns information about the segment from which this memory address was allocated.
    fn get_segment(self, size_class: SizeClass) -> MiniPageSegment {
        MiniPageSegment::from_addr(size_class, self)
    }

    /// Returns addr as an unsafe pointer gaurenteed not to be null.
    /// 
    /// # Safety
    /// The returned pointer will is known to be valid because:
    /// - It will be no earlier than the base pointer of the host heap => Pointer is not refering to memory too early in the heap, which is memory we may not control or does not exist.
    ///
    /// However the .addr field is not checked for correctness => The resulting pointer will only be safe if addr is not larger than the total host heap.
    unsafe fn as_ptr(self, base_ptr: *mut u8) -> *mut u8 {
        // # Panics
        // Should not panic because:
        // - program only supports 32 bit memory addresses => isize will be 32 bits
        // - .addr should always refer to a valid 32 bit address (up to user of AllocAddr to ensure) => .addr + base_ptr will always fit in 32 bits
        // - isize is 32 bits and resulting memory address will always fit in 32 bits => cast to isize will not fail
        base_ptr.offset(isize::try_from(self.addr).unwrap())
    }

    /// Returns the .addr field as a f64.
    fn addr_f64(self) -> f64 {
        f64::from(self.addr)
    }

    /// Returns the .addr field as a usize.
    /// # Panics
    /// usize::try_from should always work since this program only supports 32-bit addresses (aka usize will be 32 bits) and addr is u32 (32 bits).
    fn addr_usize(self) -> usize {
        usize::try_from(self.addr).unwrap()
    }
}

/// Holds metadata about a MiniPage which can be used for calculations.
/// This is different from MiniPageHeader which is a data structure which will be stored directly in the heap.
#[derive(Copy, Clone)]
struct MiniPageMeta {
    /// The index of the MiniPage within the heap. 
    page_idx: usize,

    /// The address in memory at which the page's header starts.
    header_addr: AllocAddr,

    /// The address in memory at which the page's segments start.
    segments_start_addr: AllocAddr,
}

impl MiniPageMeta {
    /// Determines MiniPageMeta information from an AllocAddr.
    fn from_addr(addr: AllocAddr) -> MiniPageMeta {
        // # Panics
        // Shouldn't panic because:
        // - program only supported with 32-bit addresses (so usize will be 32 bits) so 32 bit data will fit.
        // - dividing two f64's which are only holding u32 values => cast back to 32 bit data shouldn't overflow
        // - division result is floored so result should be an integer (this also forces any address within the page to map to the correct page)
        let page_idx: usize = usize::try_from((addr.addr_f64() / MINI_PAGE_TOTAL_BYTES_F64).floor() as u32).unwrap();

        // Determine the segment within the page
        let page_header_addr: usize = MINI_PAGE_TOTAL_BYTES_USIZE * page_idx;
        let page_segments_start_addr: usize = page_header_addr + size_of::<MiniPageHeader>();

        MiniPageMeta{
            page_idx: page_idx,
            header_addr: AllocAddr::from_usize(page_header_addr),
            segments_start_addr: AllocAddr::from_usize(page_segments_start_addr),
        }
    }

    /// Returns the page_idx as a u32.
    fn page_idx_u32(self) -> u32 {
        self.page_idx as u32
    }

    /// Returns a pointer to the MiniPage's header in the heap.
    ///
    /// # Safety
    /// TODO
    unsafe fn get_header_ptr(self, base_ptr: *mut u8) -> *mut MiniPageHeader {
        let tb = MINI_PAGE_TOTAL_BYTES_U32;
        let idx = self.page_idx_u32();
        
        let header_addr = AllocAddr::new(idx * tb);

        header_addr.as_ptr(base_ptr) as *mut MiniPageHeader
    }

    /// Returns a MiniPageSegment refering to segment_idx of size_class.
    fn get_segment(self, size_class: SizeClass, segment_idx: usize) -> MiniPageSegment {
        MiniPageSegment::from_addr(size_class, AllocAddr::from_usize(
            self.segments_start_addr.addr_usize() + (usize::from(size_class.segment_bytes()) * segment_idx)
        ))
    }
}

/// Holds information about a segment in a MiniPage.
#[derive(Copy, Clone)]
struct MiniPageSegment {
    /// Page within which segment resides.
    page: MiniPageMeta,

    /// Size class of the segment.
    size_class: SizeClass,
    
    /// The segment index within a MiniPage
    segment_idx: usize,

    /// The byte within a bit-map in which the bit for this MiniPage Segment is located.
    bitmap_byte_idx: usize,

    /// The bit within the byte refered to by bitmap_byte_idx which refers to this MiniPageSegment. Range [0, 7].
    bitmap_byte_bit_idx: usize,
}

impl MiniPageSegment {
    /// Creates a MiniPageSegment from an AllocAddr.
    fn from_addr(size_class: SizeClass, addr: AllocAddr) -> MiniPageSegment {
        // Determine the MiniPage
        let page = MiniPageMeta::from_addr(addr);

        let segment_relative_addr: usize = addr.addr_usize() - page.segments_start_addr.addr_usize();
        // # Panics
        // Shouldn't panic because:
        // - program only supports 32 bit addresses => usize will be 32 bits
        // - floor() called on result => result number will be integer
        // - converted f64s represent 32 bit data => division of the two should be 32 bits if integer
        let segment_idx_u32: u32 = ((f64::from(segment_relative_addr as u32) / f64::from(size_class.segment_bytes() as u32)).floor()) as u32;
        // # Panics
        // Shouldn't panic because:
        // - program only supports 32 bit addresses => usize will be 32 bits
        // - cast from 32 bit unsigned integer to usize should not fail => usize::try_from always = Result not Err.
        let segment_idx: usize = usize::try_from(segment_idx_u32).unwrap();

        // Determine the bitmap byte index
        // # Panics
        // Shouldn't panic because:
        // - program only supports 32 bit addresses => usize will be 32 bits
        // - dividing a u32 => usize cast to u32 shouldn't fail
        let bitmap_byte_idx: usize = usize::try_from((f64::from(segment_idx_u32) / 8.0).ceil() as u32).unwrap();
        let bitmap_byte_bit_idx: usize = segment_idx % 8;
        
        MiniPageSegment{
            page: page,
            size_class: size_class,
            segment_idx: segment_idx,
            bitmap_byte_idx: bitmap_byte_idx,
            bitmap_byte_bit_idx: bitmap_byte_bit_idx,
        }
    }

    /// Returns the start of the segment as memory address.
    fn as_addr(self) -> AllocAddr {
        let seg_start_addr = self.page.segments_start_addr.addr_usize();
        let seg_offset = self.segment_idx * usize::from(self.size_class.segment_bytes());
        
        AllocAddr::from_usize(seg_start_addr + seg_offset)
    }

    /// Returns the segment_idx as a u16.
    fn segment_idx_u16(self) -> u16 {
        // # Panics
        // Shouldn't panic because:
        // TODO
        self.segment_idx as u16
    }

    /// Write to a MiniPage's header free bitmap. Free: true = free, false = not-free.
    ///
    /// # Safety
    /// TODO
    unsafe fn write_free_bitmap(self, base_ptr: *mut u8, free: bool) {
        // Get MiniPage header
        let minipage_header = self.page.get_header_ptr(base_ptr);

        // Write
        let byte = (*minipage_header).free_segments[self.bitmap_byte_idx];
        let bit_mask: u8 = 1 << self.bitmap_byte_bit_idx;

        let new_byte: u8 = match free {
            true => {
                // Mark as free / un-allocated
                bit_mask | byte
            },
            false => {
                // Mark as used / allocated
                let not_mask = !bit_mask;
                not_mask & byte
            }
        };

        (*minipage_header).free_segments[self.bitmap_byte_idx] = new_byte;
    }

    /// Returns the segment's free status from its MiniPage header free bitmap. Returns true if free and false if not-free.
    ///
    /// Safety:
    /// TODO
    unsafe fn get_free_bitmap(self, base_ptr: *mut u8) -> bool {
        // Get the MiniPage header
        let minipage_header = self.page.get_header_ptr(base_ptr);
        
        let search_mask = 1 << self.bitmap_byte_bit_idx;
        let bit_free_status = ((*minipage_header).free_segments[self.bitmap_byte_idx] & search_mask) >> self.bitmap_byte_bit_idx;

        match bit_free_status {
            1 => true,
            _ => false,
        }
    }
}

impl AllocatorImpl<HeapType> {
    /// Initialized allocator structure with a WASMHostHeap.
    pub const INIT: AllocatorImpl<HeapType> = AllocatorImpl{
        did_init_heap: false,
        heap: UnsafeCell::new(heap::INIT),
        
        free_lists: [null_mut(); MAX_SIZE_CLASS_USIZE+1],
        meta_page: None,
        alloc_start_ptr: null_mut(),
            
        next_minipage_addr: null_mut(),

        total_alloc_reused: [0; MAX_SIZE_CLASS_USIZE+1],
        total_alloc_fresh: [0; MAX_SIZE_CLASS_USIZE+1],
        fresh_minipages: [null_mut(); MAX_SIZE_CLASS_USIZE+1],

        #[cfg(feature = "metrics")]
        failure: None,
    };
}

impl<H> AllocatorImpl<H> where H: HostHeap {
    /// Ensures that the MetaPage has been allocated and allocates the MetaPage if it has not been. Returns the existing, or newly allocated, MetaPage.
    unsafe fn ensure_meta_page(&mut self) -> *mut MetaPage {
        match self.meta_page {
            Some(p) => p,
            None => {
                // Initialize meta page
                let (p, next_ptr) = MetaPage::alloc(self.next_minipage_addr);
                self.meta_page = Some(p);

                self.alloc_start_ptr = next_ptr;

                cfg_if! {
                    if #[cfg(feature = "metrics")] {
                        // Writing MetaPage size of next_ptr - p to the heap
                        let base_ptr = (*self.heap.get()).base_ptr();
                        
                        let start_addr = AllocAddr::from_ptr(base_ptr, p as *mut u8);
                        let end_addr = AllocAddr::from_ptr(base_ptr, next_ptr);
                        
                        (*(*p).metrics).heap_bytes_write += end_addr.addr_usize() - start_addr.addr_usize();
                    }
                }

                // Set next MiniPage addr to after the allocated meta page
                self.next_minipage_addr = next_ptr;
                
                p
            },
        }
    }
    
    /// Updates a size class's free_segments stack based on the contents of a minipage's free_segments bitmap.
    /// If at least one free segment was found returns Some. The returned value is pushed onto the stack.
    /// Returns None if there were no free segments on the MiniPage.
    unsafe fn free_segments_update(&mut self, minipage: *mut MiniPageHeader) -> Option<u16> {
        let size_class = SizeClass::new((*minipage).size_class_exp);
        let meta_page = self.ensure_meta_page();

        let mut search_byte_i = 0;
        let mut first_free_found: Option<u16> = None;

        for search_bit_i in 0..size_class.segments_max_num() {
            // Check if the bit corresponding to segment search_bit_i is marked as free
            let within_byte_bit_i = search_bit_i % 8;

            let search_byte = (*minipage).free_segments[search_byte_i];
            let search_mask = 1 << within_byte_bit_i;

            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    // Reading one free_segment item from the MiniPageHeader on the heap
                    (*(*meta_page).metrics).heap_bytes_read += size_of::<u8>();
                }
            }

            let bit_free_status = (search_byte & search_mask) >> within_byte_bit_i;
            if bit_free_status == 1 {
                // If first thing found, record to return
                if first_free_found == None {
                    first_free_found = Some(search_bit_i);
                }
                
                (*(*meta_page).free_segments[size_class.exp_usize()]).push(search_bit_i);

                cfg_if! {
                    if #[cfg(feature = "metrics")] {
                        // Pushing a free segment index onto an UnsafeStack on the heap
                        (*(*meta_page).free_segments[size_class.exp_usize()]).record_push_cost(meta_page);
                    }
                }
            }
            
            // Check if last bit of the search byte, and need to retrieve next search byte from MiniPage's bitmap to look at in the next iteration
            if within_byte_bit_i == 7 {
                search_byte_i += 1;
            }
        }

        first_free_found
    }

    /// Setup a new MiniPageHead. Updates the next_minipage_addr, the free_lists head, MetaPage.free_minipages, and fresh_minipages for the size class. Always adds the new MiniPageHead to the head of free_lists.
    /// Returns Option with the created MiniPage header if there was free space in the heap.
    /// Returns None if there is no space in the heap. This is fatal.
    unsafe fn add_minipage(&mut self, size_class_exp: u8) -> Option<*mut MiniPageHeader> {
        let size_class = SizeClass::new(size_class_exp);
        let meta_page = self.ensure_meta_page();

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                (*(*meta_page).metrics).total_minipages += 1;
            }
        }
        
        // Check there is room on the heap
        let max_allowed_addr = (*self.heap.get()).base_ptr().offset(isize::from(MAX_HOST_PAGES_ISIZE * heap::PAGE_BYTES_ISIZE));
        if self.next_minipage_addr >= max_allowed_addr {
            // Out of space on the host heap
            return None;
        }

        // Determine what the next node will be
        let mut next: Option<*mut MiniPageHeader> = None;
        if !self.free_lists[size_class.exp_usize()].is_null() {
            next = Some(self.free_lists[size_class.exp_usize()]);
        }
          
        // Create new node
        let node_ptr = self.next_minipage_addr as *mut MiniPageHeader;
        (*node_ptr).next = next;
        (*node_ptr).size_class_exp = size_class_exp;
        (*node_ptr).free_segments = [255; MINI_PAGE_FREE_SEGMENTS_SIZE]; // All 1 = all unallocated

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // Writing new MiniPageHeader to the heap
                (*(*meta_page).metrics).heap_bytes_write += size_of::<MiniPageHeader>();
            }
        }

        // Set size class's free list head to new node
        self.free_lists[size_class.exp_usize()] = node_ptr;

        // Record this MiniPage as having free segments
        (*(*meta_page).free_minipages[size_class.exp_usize()]).push(node_ptr);
        (*node_ptr).on_free_minipages_stack = true;

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // Pushing MiniPageHeader pointer onto an UnsafeStack on the heap
                (*(*meta_page).free_minipages[size_class.exp_usize()]).record_push_cost(meta_page);
            }
        }

        // Set this as the current new fresh MiniPage
        self.fresh_minipages[size_class.exp_usize()] = node_ptr;

        // Increment the next MiniPageHeader address
        self.next_minipage_addr = self.next_minipage_addr.offset(isize::from(MINI_PAGE_TOTAL_BYTES));

        Some(node_ptr)
    }

    /// Allocate memory.
    unsafe fn alloc(&mut self, layout: Layout) -> *mut u8 {        
        // Don't allow 0 sized allocations
        if layout.size() == 0 {
            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    self.failure = Some(AllocFail::NoZeroAlloc);
                }
            }
            
            return null_mut();
        }

        // Check if the allocator has grabbed its pages
        // from the host yet.
        if !self.did_init_heap {
            // If the pages haven't been grabbed yet
            // Determine delta pages we need to grow by
            let current_pages = (*self.heap.get()).memory_size();
            let delta_pages = MAX_HOST_PAGES - current_pages;

            assert!(delta_pages > 0, "Shouldn't be requesting to grow the memory by a negative number");
            
            // Request the memory is grown via the host. grow_res will be the number of pages before the grow, and thus the start of our new allocated pages, or usize::MAX if error.  
            let grow_res = (*self.heap.get()).memory_grow(delta_pages);
            if grow_res == usize::MAX {
                // Failed to allocate the memory we need

                cfg_if! {
                    if #[cfg(feature = "metrics")] {
                        self.failure = Some(AllocFail::HostGrowFail);
                    }
                }
                
                // from the host
                return null_mut();
            }

            // Save the first location we can put a MiniPage
            self.next_minipage_addr = (*self.heap.get()).base_ptr();

            self.did_init_heap = true;
        }
       
        // Check Meta Page is initialized.
        let meta_page = self.ensure_meta_page();

        // Determine start of allocatable memory
        let base_ptr = self.alloc_start_ptr;

        // Determine size class of allocation
        let size_class = SizeClass::new_from_bytes(layout.size() as u16);

        // Check if not bigger than the largest MiniPage size class.
        // We don't do big alloc yet.
        if size_class.exp > MAX_SIZE_CLASS {
            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    self.failure = Some(AllocFail::BigAllocTODO);
                }
            }
            
            return null_mut();
        }

        // Record metrics
        cfg_if! {
            if #[cfg(feature = "metrics")] {
                (*(*meta_page).metrics).total_allocs[size_class.exp_usize()] += 1;
            }
        }

        // Determine if we need to allocate from a fresh or reused MiniPage
        let need_alloc_fresh = match self.total_alloc_reused[size_class.exp_usize()] > 0 {
            true => {
                let fresh_reused_ratio = f64::from(self.total_alloc_fresh[size_class.exp_usize()]) / f64::from(self.total_alloc_reused[size_class.exp_usize()]);
                fresh_reused_ratio < FRESH_REUSED_RATIO
            },
            false => false,
        };

        let node_ptr = match need_alloc_fresh {
            true => {
                // Need to allocate from a fresh minipage                
                match self.add_minipage(size_class.exp) {
                    Some(ptr) => {
                        // Put free indexes of segments on the segments stack for this new MiniPage
                        self.free_segments_update(ptr);
                        
                        ptr
                    },
                    None => {
                        // No space on host heap
                        cfg_if! {
                            if #[cfg(feature = "metrics")] {
                                self.failure = Some(AllocFail::AddMiniPageNoSpace);
                            }
                        }
                        
                        return null_mut();
                    },
                }
            },
            false => {
                // Need to try and allocate from a reused minipage
                // Find the most recently used MiniPage header for this size class
                match (*(*meta_page).free_minipages[size_class.exp_usize()]).peek() {
                    Some(ptr) => {
                        // There is a MiniPage with free segments for this size class
                        cfg_if! {
                            if #[cfg(feature = "metrics")] {
                                // For peeking the free_minipages UnsafeStack on the heap
                                (*(*meta_page).free_minipages[size_class.exp_usize()]).record_peek_cost(meta_page);
                            }
                        }

                        // If free segments stack size is 0 => the MiniPage we just peeked was just added and we haven't grabbed the free indexes from the stack yet
                        if (*(*meta_page).free_segments[size_class.exp_usize()]).size == 0 {
                            self.free_segments_update(ptr);
                        } 
                        
                        ptr
                    },
                    None => {
                        // If no MiniPage with free segments for the size class was found
                        
                        // This means we have to initialize the first MiniPage for this size class
                        // Or that there are no free MiniPages
                        match self.add_minipage(size_class.exp) {
                            Some(ptr) => {
                                // Put free indexes of segments on the segments stack for this new MiniPage
                                assert!((*(*meta_page).free_segments[size_class.exp_usize()]).size == 0, "There should be no free segment indexes left here because we didn't find a free MiniPage");

                                self.free_segments_update(ptr);
                                
                                ptr
                            },
                            None => {
                                // No space on host heap
                                cfg_if! {
                                    if #[cfg(feature = "metrics")] {
                                        self.failure = Some(AllocFail::AddMiniPageNoSpace);
                                    }
                                }
                                
                                return null_mut();
                            },
                        }
                    },
                }
            },
        };

        assert!(!node_ptr.is_null(), "A MiniPageHeader should have been found at which to begin the search for a free segment to allocate");
        assert!(!self.free_lists[size_class.exp_usize()].is_null(), "Since a MiniPageHeader to begin the search was found, the head of this size class's free free list should not be null");

        // Find the next free segment
        let next_free_segment_idx: u16 = match (*(*meta_page).free_segments[size_class.exp_usize()]).pop() {
            Some(idx) => {
                 // The free segments stack for the MiniPage had segments on it
                cfg_if! {
                    if #[cfg(feature = "metrics")] {
                        // For popping a segment index of an UnsafeStack in the heap
                        (*(*meta_page).free_segments[size_class.exp_usize()]).record_pop_cost(meta_page);
                    }
                }
                
                idx
            },
            None => {
                // Fatal error: There are no free segments in the current MiniPage. This should not occur! As the current MiniPage was taken off of free_minipages. A stack where only MiniPages with free segments are stored.
                cfg_if! {
                    if #[cfg(feature = "metrics")] {
                        self.failure = Some(AllocFail::FreeMiniPagesContractBreach);
                    }
                }

                
                return null_mut();
            },
        };

        assert!(!node_ptr.is_null(), "node_ptr should not be null");

        // Count allocation as either using a reused MiniPage or a fresh MiniPage
        // We must do this before the next block, where fresh_minipages is potentially reset.
        if self.fresh_minipages[size_class.exp_usize()] == node_ptr {
            self.total_alloc_fresh[size_class.exp_usize()] += 1;
        } else {
            self.total_alloc_reused[size_class.exp_usize()] += 1;
        }

        // Determine if the MiniPage we just got a free segment index from still has free space after this allocation
        if (*(*meta_page).free_segments[size_class.exp_usize()]).size == 0 {
            // After this allocation this MiniPage will no longer have any free segments
            // Remove from free_minipages
            (*(*meta_page).free_minipages[size_class.exp_usize()]).pop();
            
            (*node_ptr).on_free_minipages_stack = false;

            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    // For popping a MiniPage of an UnsafeStack in the heap
                    (*(*meta_page).free_minipages[size_class.exp_usize()]).record_pop_cost(meta_page);

                    // For setting the on_free_minipages_stack field on a MiniPageHeader in the heap
                    (*(*meta_page).metrics).heap_bytes_write += size_of::<bool>();
                }
            }

            // If this MiniPage was also considered "fresh" then unmark as fresh
            if self.fresh_minipages[size_class.exp_usize()] == node_ptr {
                self.fresh_minipages[size_class.exp_usize()] = null_mut();
            }
        }

        // Determine address we will allocate
        let page_addr = AllocAddr::from_ptr(base_ptr, node_ptr as *mut u8);
        let page_meta = MiniPageMeta::from_addr(page_addr);
        let segment = page_meta.get_segment(size_class, usize::from(next_free_segment_idx));

        // Mark segment as not free
        segment.write_free_bitmap(base_ptr, false);

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // For writing to a MiniPageHeader free_segments byte on the heap
                (*(*meta_page).metrics).heap_bytes_write += size_of::<bool>();
            }
        }

        // assert!(false,  "alloc made node_ptr={:?}", *node_ptr);

        // Return address
        segment.as_addr().as_ptr(base_ptr)
    }

    unsafe fn dealloc(&mut self, ptr: *mut u8, _layout: Layout) {
        // Get some information about the heap
        let meta_page = self.ensure_meta_page();
        let base_ptr = self.alloc_start_ptr;

        let addr = AllocAddr::from_ptr(base_ptr, ptr);
        let page_meta = MiniPageMeta::from_addr(addr);

        // Read the size class
        let minipage_header_addr = AllocAddr::new(page_meta.page_idx_u32() * MINI_PAGE_TOTAL_BYTES_U32);
        let minipage_header = minipage_header_addr.as_ptr(base_ptr) as *mut MiniPageHeader;

        // assert!(false,  "dealloc found minipage_header={:?}", *minipage_header);

        let size_class = SizeClass::new((*minipage_header).size_class_exp);

        // Record metrics
        cfg_if! {
            if #[cfg(feature = "metrics")] {
                (*(*meta_page).metrics).total_deallocs[size_class.exp_usize()] += 1;
            }
        }

        // Determine segment
        let segment = addr.get_segment(size_class);

        // Ensure segment was previously allocated
        if segment.get_free_bitmap(base_ptr) {
            // Segment not allocated
            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    // For reading from a MiniPageHeader free_segments byte on the heap
                    (*(*meta_page).metrics).heap_bytes_read += size_of::<bool>();
                }
            }
            
            return;
        }

        // Update segment bitmap
        segment.write_free_bitmap(base_ptr, true);

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // For writing to a MiniPageHeader free_segments byte on the heap
                (*(*meta_page).metrics).heap_bytes_write += size_of::<bool>();
            }
        }

        // Push onto free segments stack if minipage is the current MiniPage
        if (*(*meta_page).free_minipages[size_class.exp_usize()]).peek() == Some(minipage_header) {
            (*(*meta_page).free_segments[size_class.exp_usize()]).push(segment.segment_idx_u16());

            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    // For peeking the free_minipages UnsafeStack on the heap
                    (*(*meta_page).free_minipages[size_class.exp_usize()]).record_peek_cost(meta_page);
                    
                    // For pushing a free segment onto the free_segments UnsafeStack on the heap
                    (*(*meta_page).free_segments[size_class.exp_usize()]).record_push_cost(meta_page);
                }
            }
        } else if !(*minipage_header).on_free_minipages_stack {
            // Not pushed on minipages stack
            // First time we have deallocated from this MiniPage since it was full
            
            (*(*meta_page).free_minipages[size_class.exp_usize()]).push(minipage_header);
            
            cfg_if! {
                if #[cfg(feature = "metrics")] {
                    // For pushing a MiniPageHeader pointer onto the free_minipages UnsafeStack on the heap
                    (*(*meta_page).free_minipages[size_class.exp_usize()]).record_push_cost(meta_page);
                }
            }
        }

        cfg_if! {
            if #[cfg(feature = "metrics")] {
                // For reading the (*minipage_header).on_free_minipages_stack bool from the heap
                (*(*meta_page).metrics).heap_bytes_read += size_of::<bool>();
            }
        }
    }
}

/// The custom global allocator. Wraps the AllocatorImpl
/// which performs all memory operations. See for more details.
pub struct AlligatorAlloc<H> where H: HostHeap {
    /// Data structure which keeps state of all memory
    /// wrapped inside an UnsafeCell for
    /// memory symantics.
    alloc: UnsafeCell<AllocatorImpl<H>>,
}

/// WASM is single threaded right now so this should be okay.
unsafe impl<H> Sync for AlligatorAlloc<H> where H: HostHeap {}

/// Includes statistics on which allocations were made from MiniPages which were fresh (never been fully filled up) or reused (been fully filled up, then freed into action again).
#[derive(Copy, Clone, Debug)]
pub struct FreshReusedStats {
    /// Total number of allocations for each size class which were performed from a reused MiniPage header.
    pub total_alloc_reused: [u32; MAX_SIZE_CLASS_USIZE + 1],

    /// Total number of allocations for each size class which were performed from a newly allocated MiniPage header.
    pub total_alloc_fresh: [u32; MAX_SIZE_CLASS_USIZE + 1],
}

impl AlligatorAlloc<HeapType> {
    pub const INIT: AlligatorAlloc<HeapType> = AlligatorAlloc{
        alloc: UnsafeCell::new(AllocatorImpl::INIT),
    };

    pub unsafe fn fresh_reused_stats(&self) -> FreshReusedStats {
        FreshReusedStats{
            total_alloc_reused: (*self.alloc.get()).total_alloc_reused,
            total_alloc_fresh: (*self.alloc.get()).total_alloc_fresh,
        }
    }

    cfg_if! {
        if #[cfg(feature = "metrics")] {
            /// Returns metrics about the allocation process. None if the allocator hasn't run or setup the metrics recording mechanism yet.
            pub unsafe fn metrics(&self) -> Option<AllocMetrics> {
                match (*self.alloc.get()).meta_page {
                    Some(meta_page) => Some(*(*meta_page).metrics),
                    None => None,
                }
            }

            /// Returns the allocation failure cause.
            pub unsafe fn alloc_failure_cause(&self) -> Option<AllocFail> {
                (*self.alloc.get()).failure
            }
        }
    }
}

unsafe impl<H> GlobalAlloc for AlligatorAlloc<H> where H: HostHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        return (*self.alloc.get()).alloc(layout);
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        (*self.alloc.get()).dealloc(ptr, layout);
    }
}
