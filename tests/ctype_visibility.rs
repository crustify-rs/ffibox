//! `define_ctype!` with a restricted visibility: the layout type and its
//! handles get it, so they may mirror a crate-private C type.

#![deny(unreachable_pub, private_interfaces)]

use core::marker::PhantomData;

use ffibox::{CCell, CRefSeam};

mod bindings {
    #[repr(C)]
    pub(crate) struct node_st {
        pub(crate) id: u32,
    }

    #[repr(C)]
    pub(crate) struct view_st {
        pub(crate) len: usize,
    }
}

ffibox::define_ctype!(pub(crate) Node, NodeRef, NodeMut, bindings::node_st);
ffibox::define_ctype!(
    pub(crate) View<'s>, ViewRef, ViewMut, bindings::view_st,
    phantom = PhantomData<&'s [u8]>,
);

#[test]
fn restricted_types_mirror_crate_private_c_types() {
    let node = Node(bindings::node_st { id: 7 }, PhantomData);
    let r: NodeRef<'_> = node.as_ref();
    let p: *const <Node as CCell>::C = r.as_ptr();
    // SAFETY: `p` points to `node`, alive and unwritten.
    assert_eq!(unsafe { (*p).id }, 7);

    let view = View(bindings::view_st { len: 3 }, PhantomData);
    let v: ViewRef<'_, '_> = view.as_ref();
    // SAFETY: as above.
    assert_eq!(unsafe { (*v.as_ptr()).len }, 3);
}
