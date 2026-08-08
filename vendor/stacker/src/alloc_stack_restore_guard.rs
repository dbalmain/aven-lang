use crate::{StackError, get_stack_limit, set_stack_limit};

pub struct StackRestoreGuard {
    new_stack: *mut u8,
    stack_bytes: usize,
    old_stack_limit: Option<usize>,
}

const ALIGNMENT: usize = 16;

impl StackRestoreGuard {
    pub fn new(stack_bytes: usize) -> Result<StackRestoreGuard, StackError> {
        // On these platforms we do not use stack guards. this is very unfortunate,
        // but there is not much we can do about it without OS support.
        // We simply allocate the requested size from the global allocator with a suitable
        // alignment.
        let stack_bytes = stack_bytes
            .checked_add(ALIGNMENT - 1)
            .ok_or_else(|| StackError::invalid_input("unreasonably large stack requested"))?
            / ALIGNMENT
            * ALIGNMENT;
        let layout = std::alloc::Layout::from_size_align(stack_bytes, ALIGNMENT)
            .map_err(|_| StackError::invalid_input("unreasonably large stack requested"))?;
        let ptr = unsafe { std::alloc::alloc(layout) };
        if ptr.is_null() {
            return Err(StackError::last_os_error("unable to allocate stack"));
        }
        Ok(StackRestoreGuard {
            new_stack: ptr,
            stack_bytes,
            old_stack_limit: get_stack_limit(),
        })
    }

    pub fn stack_area(&self) -> (*mut u8, usize) {
        (self.new_stack, self.stack_bytes)
    }
}

impl Drop for StackRestoreGuard {
    fn drop(&mut self) {
        unsafe {
            std::alloc::dealloc(
                self.new_stack,
                std::alloc::Layout::from_size_align_unchecked(self.stack_bytes, ALIGNMENT),
            );
        }
        set_stack_limit(self.old_stack_limit);
    }
}
