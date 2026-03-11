# Bootstrappable Buddy Allocator

This crate contains a [buddy system allocator](https://en.wikipedia.org/wiki/Buddy_memory_allocation) backend.
The `BuddyAllocator` does not implement `Allocator`, This is to allow it to be used to manage more than virtual memory. 
The user must implement their own front end.

This implementation is designed with the intention of bootstrapping, that is to be able to be initialized with minimal 
help from other memory allocators.

Other implementations written in rust are limited by either being a fixed size, or not being able to be bootstrapped. 
This implementation can be used to manage an arbitrary amount of memory, with holes, that is not known at compile or start-time. 