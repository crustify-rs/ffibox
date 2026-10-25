//! An upstream wrapper crate: it invokes `define_ctype!`, so the handles'
//! inherent seam is `pub(crate)` here and invisible to `cross_crate_seam.rs`.

/// Stand-in for a `-sys` struct.
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct widget_st {
    /// A field C reads and writes.
    pub size: u32,
}

ffibox::define_ctype!(
    /// The wrapped `widget_st`.
    Widget,
    WidgetRef,
    WidgetMut,
    widget_st
);

// SAFETY: any `size` is valid, zero included.
unsafe impl ffibox::CZeroable for Widget {}

impl WidgetRef<'_> {
    /// Reads `size` through the crate-private seam.
    #[must_use]
    pub fn size(&self) -> u32 {
        // SAFETY: a read through the shared handle's pointer, live for `'_`.
        unsafe { core::ptr::addr_of!((*self.as_ptr()).size).read() }
    }
}

impl WidgetMut<'_> {
    /// Writes `size` through the crate-private seam.
    pub fn set_size(&mut self, v: u32) {
        // SAFETY: a write through the exclusive handle's pointer.
        unsafe { core::ptr::addr_of_mut!((*self.as_mut_ptr()).size).write(v) }
    }
}
