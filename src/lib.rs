#![feature(allocator_api)]
#![no_std]
extern crate alloc;

use core::alloc::{AllocError, Allocator};
use core::mem::MaybeUninit;
use binary_search_tree::BinarySearchTree;

/// This is the backend buddy allocator.
///
/// [Mode of operation](https://en.wikipedia.org/wiki/Buddy_memory_allocation).
/// This implementation internally uses a series of binary trees to track the free lists.
///
/// * `ORDERS` indicates the number of free lists. This with `PAGE_SIZE` indicates the maximum size
/// allocation this can handle. A smaller `ORDERS` reduces the maximum time complexity of operations
/// but increases the memory complexity.
/// * `PAGE_SIZE` indicates the smallest unit this will return. This is a bit offset not a raw value.
/// An actual page size of 4K requires this to be set to `12` (`1 << 12 == 4K`).
/// * `O: OverflowMode`. This indicates how the `BuddyAllocator` will handle max order overflows.
/// When `O` is [Overflow], then [Self::deallocate] will perform a buddy check on the highest order
/// and if the budy is found it will return with an error contain the bock address.
/// This allows a higher level allocator to manage these sections of memory.
/// When `O` is [NoOverflow] then the highest order will never be buddy checked, and
/// [Self::deallocate] will never return with an error.
/// * `T` & `M` Is a representation of the memory types that will be used. This allows the BuddyAllocator to be used for more than [Allocator],
/// it can be used as a physical memory allocator.
/// * `A` Is the backing allocator. This will only be used to allocate/free one node at a time.
/// A simple implementation like [linked_list_allocator](https://crates.io/crates/linked_list_allocator) can be used.
///
/// The maximum size this allocator allocate/free is `1 << (PAGE_SIZE_OFFSET + ORDERS)`.
#[allow(private_bounds)]
pub struct BuddyAllocator<const ORDERS: usize, const PAGE_SIZE: usize, O: OverflowMode, T, M, A>
    where
        M: memory_addresses::MemoryAddress<RAW=T> + 'static,
        A: Allocator + Clone + 'static,
        T: From<u8> + Copy


{
    orders: [Order<T,M,A>; ORDERS],
    _p: core::marker::PhantomData<O>
}

#[allow(private_bounds)]
impl<const ORDERS: usize, const PAGE_SIZE_OFFSET: usize, O: OverflowMode, T, M, A> BuddyAllocator<ORDERS, PAGE_SIZE_OFFSET, O, T, M, A>
    where
        M: memory_addresses::MemoryAddress<RAW=T> + 'static,
        A: Allocator + Clone + Copy + 'static,
        T: From<u8> + Copy
{
    pub const fn new(alloc: A) -> Self {

        // Order is not `Copy` and the length of `orders` is not known at writing-time, and `new` needs to be const.
        // This constructs an array of ORDERS size with uninit elements and initializes them at compile time.
        // This is an annoying trick to initialize the whole array at compile time.
        let mut orders: [MaybeUninit<Order<T, M, A>>; ORDERS] = [ const { MaybeUninit::uninit() }; ORDERS];
        let mut count = 0;
        while count < ORDERS {
            orders[count].write(Order::new(alloc));
            count += 1;
        }

        Self {
            // SAFETY: This is safe because `orders` contains fully initialized `Order`'s
            orders: unsafe { core::mem::transmute_copy(&orders) },
            _p: core::marker::PhantomData
        }
    }

    /// [Order] requires that addresses are shifted,
    /// so the difference between a buddy at every order is `1`.
    ///
    /// For example with `PAGE_SIZE_OFFSET` is `12` and an ordering `0`, the address `4096` will be `1`.
    fn encode_addr(addr: M, order: usize) -> Encoded<M> {
        Encoded(addr >> (PAGE_SIZE_OFFSET + order))
    }

    /// See [Self::encode_addr]
    fn decode_addr(addr: Encoded<M>, order: usize) -> M {
        let Encoded(addr) = addr;
        addr << (PAGE_SIZE_OFFSET + order)
    }

    /// Allocates `size`, returning `Ok(_)` if self contains enough memory.
    ///
    /// A size of `0` will be treated as a size of `1<<PAGE_SIZE_OFFSET`, the caller should sanitize size `0`.
    /// Internally the size is calculated using `(size >> PAGE_SIZE_OFFSET).next_power_of_two()`,
    /// which determines the starting order. If the size is not aligned to `1 << PAGE_SIZE_OFFSET`
    /// it will be incremented by that amount to ensure the starting order is large enough.
    ///
    /// ## Panics
    ///
    /// If size is larger than the maximum allocation size this fn will panic.
    pub fn allocate(&mut self, size: usize) -> Result<M,AllocError> {
        let offset = Self::calculate_offset(size);
        let order = offset.trailing_zeros() as usize;
        self.allocate_inner(order)
    }

    fn calculate_offset(size: usize) -> usize {
        if size & (PAGE_SIZE_OFFSET - 1) == 0 {
            (size >> PAGE_SIZE_OFFSET).next_power_of_two()
        } else {
            // Increment size by one page if `size` is not page aligned.
            ((size + (1 << PAGE_SIZE_OFFSET)) >> PAGE_SIZE_OFFSET).next_power_of_two() // Bump the address up by one page
        }
    }

    fn allocate_inner(&mut self, order: usize) -> Result<M,AllocError> {

        match self.orders[order].pop() {
            Some(addr) => Ok(Self::decode_addr(addr, order)),
            None if order+1 == ORDERS => Err(AllocError),
            None => {
                let addr = self.allocate_inner(order+1)?;
                let remain = buddy_of(Self::encode_addr(addr, order));
                if let OperationResult::Merged(_) = self.orders[order].insert(remain) {
                    // The buddy of the block allocated from the higher order is currently present in this order, which should not be possible.
                    panic!();
                };
                Ok(addr)
            }
        }
    }
}

macro_rules! impl_dealloc {
    ($overflow:ty, $self:ident, $addr:ident, $order:ident, $last_order:expr) => {
        impl<const ORDERS: usize, const PAGE_SIZE_OFFSET: usize, T, M, A> BuddyAllocator<ORDERS, PAGE_SIZE_OFFSET, $overflow, T, M, A>
        where
        M: memory_addresses::MemoryAddress<RAW=T> + 'static,
        A: Allocator + Clone + Copy + 'static,
        T: From<u8> + Copy
        {
            /// Frees the requested address with the given size.
            /// The caller must use the same `size` sanitizations as the ones used for [Self::allocate].
            ///
            /// This function may allocate memory from `A`.
            ///
            /// If `O` is [Overflow] this may return `Err(address)`, the address is a block of `1 << (PAGE_SIZE_OFFSET + ORDERS) * 2`
            /// If `O` is [NoOverflow] this fn will never return `Err(_)`.
            ///
            /// ## Panics
            ///
            /// If size is larger than the maximum allocation size this fn will panic.
            pub fn deallocate(&mut self, size: usize, addr: M) -> Result<(),M> {
                let offset = Self::calculate_offset(size);
                let order = offset.trailing_zeros() as usize;
                self.deallocate_inner(order, addr)
            }

            fn deallocate_inner(&mut self, $order: usize, address: M) -> Result<(),M> {
                let $self = self;
                match $self.orders[$order].insert(Self::encode_addr(address, $order)) {
                    OperationResult::Success => {Ok(())}
                    OperationResult::Merged($addr) if $order+1 == ORDERS => {
                        $last_order
                    }
                    OperationResult::Merged(m) => {
                        $self.deallocate_inner($order + 1, Self::decode_addr(m,$order))?;
                        Ok(())
                    }
                }
            }
        }
    };
}

impl_dealloc!(Overflow, _i,m,order, Err(Self::decode_addr(m,order)));
impl_dealloc!(NoOverflow, this ,m,order, {
    this.orders[order].insert_without_buddy_check(m);
    Ok(())
});

/// Contains the elements of a single order.
///
/// Addresses must be given as order indices so the difference between each buddy is `1`.
/// e.g. for a page size 4K order 2 the `address >> 14`
struct Order<T,M,A>
where
    M: memory_addresses::MemoryAddress<RAW=T> + 'static,
    A: Allocator + Clone + 'static
{
    binary_search_tree: BinarySearchTree<Encoded<M>,A>
}

impl<T,M,A> Order<T,M,A>
    where
        M: memory_addresses::MemoryAddress<RAW=T> + 'static,
        A: Allocator + Clone + Copy + 'static,
        T: From<u8>
{
    const fn new(alloc: A) -> Order<T,M,A> {
        Self {
            binary_search_tree: BinarySearchTree::new_in(alloc)
        }
    }

    /// Attempts to insert `address`.
    ///
    /// If the buddy for this address is found then it will be removed and [OperationResult::Merged] will be returned.
    #[must_use]
    fn insert(&mut self, address: Encoded<M>) -> OperationResult<M> {
        let buddy = buddy_of(address);
        if self.binary_search_tree.contains(&buddy) {
            self.binary_search_tree.remove(&buddy);
            OperationResult::Merged(buddy.min(address))
        } else {
            assert!(!self.binary_search_tree.insert_without_dup(address), "Duplicate address in order");
            OperationResult::Success
        }
    }

    fn insert_without_buddy_check(&mut self, address: Encoded<M>) {
        assert!(!self.binary_search_tree.insert_without_dup(address));
    }

    #[must_use]
    fn pop(&mut self) -> Option<Encoded<M>> {
        self.binary_search_tree.extract_max()
    }
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Encoded<M>(M);

fn buddy_of<M,T>(address: Encoded<M>) -> Encoded<M>
where
    M: memory_addresses::MemoryAddress<RAW=T> + 'static,
    T: From<u8>
{
    let Encoded(mut address) = address;
    let one: T = 1u8.into();
    address ^= one;
    Encoded(address)
}

enum OperationResult<M: memory_addresses::MemoryAddress> {
    Success,
    /// On insertion this indicates that the buddy was found and was removed.
    ///
    /// The value contained in this variant is the lower of the values
    /// which should be inserted into the higher order.
    Merged(Encoded<M>),
}

trait OverflowMode {}

pub enum Overflow {}
impl OverflowMode for Overflow {}
pub enum NoOverflow {}
impl OverflowMode for NoOverflow {}

#[cfg(test)]
mod tests {
    use alloc::alloc::Global;
    use alloc::vec::Vec;
    use rand::prelude::SliceRandom;
    use super::*;
    use core::alloc::Layout;
    use core::ptr::NonNull;
    extern crate std;
    extern crate alloc;

    type TestBAlloc<const ORDERS: usize, O> = BuddyAllocator::<ORDERS,12, O,u64,memory_addresses::arch::x86_64::VirtAddr,Global>;


    struct ImplAlloc<const ORDERS: usize, O: OverflowMode> {
        inner: std::cell::RefCell<TestBAlloc<ORDERS,O>>
    }

    /// # Safety
    ///
    /// Note that all allocations and frees are unsound, because this does not manage system memory.
    /// Allocated addresses returned by `allocate` must not be dereferenced.
    unsafe impl<const ORDERS: usize> Allocator for ImplAlloc<ORDERS,Overflow> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let mut l = self.inner.borrow_mut();
            let size = layout.size().min(layout.align());
            l.allocate(size).map(|r| {
                let ptr = r.as_mut_ptr();
                let Some(v) = NonNull::new(unsafe { core::slice::from_raw_parts_mut(ptr, size.next_power_of_two()) }) else {
                    panic!()
                };
                v
            })
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            let mut l = self.inner.borrow_mut();
            let size = layout.size().min(layout.align());
            l.deallocate(size, memory_addresses::arch::x86_64::VirtAddr::from_ptr(ptr.as_ptr())).expect("Exceeded single block size");
        }
    }

    unsafe impl<const ORDERS: usize> Allocator for ImplAlloc<ORDERS,NoOverflow> {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let mut l = self.inner.borrow_mut();
            let size = layout.size().min(layout.align());
            l.allocate(size).map(|r| {
                let ptr = r.as_mut_ptr();
                let Some(v) = NonNull::new(unsafe { core::slice::from_raw_parts_mut(ptr, size.next_power_of_two()) }) else {
                    panic!()
                };
                v
            })
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            let mut l = self.inner.borrow_mut();
            let size = layout.size().min(layout.align());
            l.deallocate(size, memory_addresses::arch::x86_64::VirtAddr::from_ptr(ptr.as_ptr())).expect("Exceeded single block size");
        }
    }

    #[test]
    fn buddy_calc() {
        let base = memory_addresses::arch::x86_64::VirtAddr::new(0);
        let encoded = TestBAlloc::<12,Overflow>::encode_addr(base,0);
        assert_eq!(encoded.0, base);
        let buddy = buddy_of(encoded);
        assert_eq!(buddy_of(buddy), encoded);
    }

    #[test]
    fn bootstrap() {
        let alloc: ImplAlloc<11, NoOverflow> = ImplAlloc {
            inner: std::cell::RefCell::new(TestBAlloc::new(Global))
        };
        for n in (0..0x1000_0000usize).step_by(0x1000).skip(1) {
            unsafe { alloc.deallocate( NonNull::new(n as *mut u8).unwrap(), Layout::from_size_align_unchecked(0x1000, 0x1000)) };
        }
    }

    #[test]
    #[should_panic(expected = "Exceeded single block size")]
    fn bootstrap_overflow() {
        let alloc: ImplAlloc<11, Overflow> = ImplAlloc {
            inner: std::cell::RefCell::new(TestBAlloc::new(Global))
        };
        for n in (0..0x1000_0000usize).step_by(0x1000).skip(1) {
            unsafe { alloc.deallocate( NonNull::new(n as *mut u8).unwrap(), Layout::from_size_align_unchecked(0x1000, 0x1000)) };
        }
    }

    #[test]
    fn allocate_simple() {
        let bs = 0x100_0000usize;
        let alloc: ImplAlloc<14, Overflow> = ImplAlloc {
            inner: std::cell::RefCell::new(TestBAlloc::new(Global))
        };
        unsafe { alloc.deallocate(NonNull::new(bs as *mut u8).unwrap(), Layout::from_size_align_unchecked(bs, bs)) };

        for _ in 0..0x1000 {
            alloc.allocate(unsafe { Layout::from_size_align_unchecked(4096, 4096) }).unwrap();
        }
    }

    #[test]
    fn alloc_not_simple() {
        let bs = 0x100_0000usize;
        let alloc: ImplAlloc<14, NoOverflow> = ImplAlloc {
            inner: std::cell::RefCell::new(TestBAlloc::new(Global))
        };
        for i in 0..16 {
            unsafe { alloc.deallocate(NonNull::new((bs*(i+2)) as *mut u8).unwrap(), Layout::from_size_align_unchecked(bs, bs)) };
        }

        let mut rng = rand::rng();
        let mut v = Vec::new();
        for _ in 0..0x8000 {
            let ptr = alloc.allocate(unsafe { Layout::from_size_align_unchecked(4096, 4096) }).unwrap();
            v.push(ptr);
        }

        for _i in 0..1_0000 {

            v.shuffle(&mut rng);
            for _ in 0..0x4000 {
                unsafe { alloc.deallocate(v.pop().unwrap().cast(), Layout::from_size_align_unchecked(4096, 4096)) };
            }

            for _ in 0..0x4000 {
                v.push(alloc.allocate(unsafe { Layout::from_size_align_unchecked(4096, 4096) }).unwrap())
            }

            if !has_unique_elements(&v) {
                std::println!("----Borked----");
                v.sort();
                for i in v {
                    std::println!("{:p}", i.as_ptr().cast::<u8>())
                }
                panic!("Contains duplicate elements");
            }
        }
    }

    #[test]
    fn big_test() {
        let bs = 0x100_0000usize;
        let alloc: ImplAlloc<14, NoOverflow> = ImplAlloc {
            inner: std::cell::RefCell::new(TestBAlloc::new(Global))
        };

        unsafe { alloc.deallocate(NonNull::new(bs as *mut u8).unwrap(), Layout::from_size_align_unchecked(bs, bs)) };
        let out = alloc.allocate(unsafe { Layout::from_size_align_unchecked(4096, 4096) }).unwrap();

        for i in &alloc.inner.borrow().orders {
            for j in i.binary_search_tree.reverse_order() {
                std::print!("{:?}", j);
            }
            std::println!();
        }

        unsafe { alloc.deallocate(out.cast(), Layout::from_size_align_unchecked(4096, 4096)) };


        for i in &alloc.inner.borrow().orders {
            for j in i.binary_search_tree.reverse_order() {
                std::print!("{:?}", j);
            }
            std::println!();
        }
    }

    fn has_unique_elements<T>(iter: T) -> bool
    where
        T: IntoIterator,
        T::Item: Eq + std::hash::Hash + Copy + core::fmt::Debug,
    {
        let mut uniq = std::collections::HashSet::new();
        iter.into_iter().all(move |x| {
            let rc = uniq.insert(x);
            if !rc {
                std::println!("Duplicate element found {:?}", x);
            }
            rc
        })
    }
}