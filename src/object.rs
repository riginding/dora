use std;
use std::ffi::CString;
use std::marker::PhantomData;
use std::ops::{Deref, DerefMut};
use std::ptr;
use std::slice;

use class::ClassId;
use ctxt::{SemContext, get_ctxt};
use gc::root::IndirectObj;
use mem;
use vtable::VTable;

#[repr(C)]
pub struct Header {
    // ptr to class
    vtable: *mut VTable,

    // additional information>
    // bit 0 - marked flag
    // bit 1-4 - age (4 bits)
    info: usize,
}

impl Header {
    pub fn size() -> i32 {
        std::mem::size_of::<Header>() as i32
    }

    pub fn vtblptr(&self) -> *mut VTable {
        self.vtable
    }

    pub fn vtbl(&self) -> &mut VTable {
        unsafe { &mut *self.vtable }
    }

    pub fn set_mark(&mut self, value: bool) {
        self.info = if value {
            self.info | 1
        } else {
            self.info & (!1)
        };
    }

    pub fn marked(&self) -> bool {
        if (self.info & 1) != 0 { true } else { false }
    }

    pub fn age(&self) -> u32 {
        (self.info & 0x1E) as u32 >> 1
    }

    pub fn set_age(&mut self, age: u32) {
        assert!(age <= 15);
        self.info = (self.info & !0x1E) | (age << 1) as usize;
    }
}

#[test]
fn test_age() {
    let hdr = Header {
        vtable: ptr::null_mut(),
        info: 3 << 1,
    };
    assert_eq!(3, hdr.age());

    let hdr = Header {
        vtable: ptr::null_mut(),
        info: 15 << 1,
    };
    assert_eq!(15, hdr.age());

    let hdr = Header {
        vtable: ptr::null_mut(),
        info: 0,
    };
    assert_eq!(0, hdr.age());

    let hdr = Header {
        vtable: ptr::null_mut(),
        info: 0xFFFF,
    };
    assert_eq!(15, hdr.age());
}

#[test]
fn test_set_age() {
    let mut hdr = Header {
        vtable: ptr::null_mut(),
        info: 0,
    };

    hdr.set_age(15);
    assert_eq!(15, hdr.age());

    hdr.set_age(0);
    assert_eq!(0, hdr.age());

    hdr.set_age(2);
    assert_eq!(2, hdr.age());
}

#[test]
#[should_panic]
fn test_set_age_over_15() {
    let mut hdr = Header {
        vtable: ptr::null_mut(),
        info: 0,
    };
    hdr.set_age(16);
}

// is used to reference any object
#[repr(C)]
pub struct Obj {
    header: Header,
    data: u8,
}

impl Obj {
    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut Header {
        &mut self.header
    }

    pub fn data(&self) -> *const u8 {
        &self.data as *const u8
    }

    pub fn size(&self) -> usize {
        let cls = self.header().vtbl().class();
        let size = cls.size;

        if size > 0 {
            return size as usize;
        }

        let ty = cls.ty;

        if cls.is_array {
            let handle: Handle<ByteArray> = Handle { ptr: self as *const Obj as *const ByteArray };

            let value = Header::size() as usize + mem::ptr_width() as usize +
                        cls.element_size as usize * handle.len() as usize;

            return mem::align_usize(value, mem::ptr_width() as usize);
        }

        if ty.cls_id() == get_ctxt().primitive_classes.str_class {
            let handle: Handle<Str> = Handle { ptr: self as *const Obj as *const Str };
            mem::align_usize(handle.size(), mem::ptr_width() as usize)
        } else {
            panic!("size unknown");
        }
    }

    pub fn visit_reference_fields<F>(&mut self, mut f: F)
        where F: FnMut(IndirectObj)
    {
        let classptr = self.header().vtbl().classptr;
        let cls = unsafe { &*classptr };

        if cls.is_object_array {
            let array = unsafe { &*(self as *const _ as *const StrArray) };

            // walk through all objects in array
            let mut ptr = array.data() as *mut *mut Obj;
            let last = unsafe { ptr.offset(array.len() as isize) };

            while ptr < last {
                f((ptr as usize).into());

                unsafe { ptr = ptr.offset(1) }
            }

            return;
        }

        for &offset in &cls.ref_fields {
            let obj = (self as *mut Obj as usize) + offset as usize;
            f(obj.into());
        }
    }
}

#[repr(C)]
pub struct Handle<T> {
    ptr: *const T,
}

unsafe impl<T> Send for Handle<T> {}

impl<T> Handle<T> {
    pub fn null() -> Handle<T> {
        Handle { ptr: ptr::null() }
    }

    pub fn cast<R>(&self) -> Handle<R> {
        Handle { ptr: self.ptr as *const R }
    }

    pub fn raw(&self) -> *const T {
        self.ptr
    }
}

// known limitation of #[derive(Copy, Clone)]
// traits need to be implemented manually
impl<T> Copy for Handle<T> {}
impl<T> Clone for Handle<T> {
    fn clone(&self) -> Handle<T> {
        *self
    }
}

impl<T> Deref for Handle<T> {
    type Target = T;

    fn deref(&self) -> &T {
        unsafe { &*self.ptr }
    }
}

impl<T> DerefMut for Handle<T> {
    fn deref_mut(&mut self) -> &mut T {
        unsafe { &mut *(self.ptr as *mut T) }
    }
}

impl<T> Into<Handle<T>> for usize {
    fn into(self) -> Handle<T> {
        Handle { ptr: self as *const T }
    }
}

#[repr(C)]
pub struct Testing {
    header: Header,
    failed: bool,
}

impl Testing {
    pub fn has_failed(&self) -> bool {
        self.failed
    }
}

#[repr(C)]
pub struct Str {
    header: Header,
    length: usize,
    data: u8,
}

impl Str {
    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut Header {
        &mut self.header
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn data(&self) -> *const u8 {
        &self.data as *const u8
    }

    pub fn to_cstring(&self) -> CString {
        let view = unsafe { slice::from_raw_parts(self.data(), self.len()) };

        CString::new(view).unwrap()
    }

    pub fn size(&self) -> usize {
        Header::size() as usize         // Object header
            + mem::ptr_width() as usize // length field
            + self.len() + 1 // string content
    }

    /// allocates string from buffer in permanent space
    pub fn from_buffer_in_perm(ctxt: &SemContext, buf: &[u8]) -> Handle<Str> {
        let mut handle = str_alloc_perm(ctxt, buf.len());
        handle.length = buf.len();

        unsafe {
            let data = handle.data() as *mut u8;

            // copy buffer content into Str
            ptr::copy_nonoverlapping(buf.as_ptr(), data, buf.len());

            // string should end with 0 for C compatibility
            *(data.offset(buf.len() as isize)) = 0;
        }

        handle
    }

    /// allocates string from buffer in permanent space
    pub fn from_buffer(ctxt: &SemContext, buf: &[u8]) -> Handle<Str> {
        let mut handle = str_alloc_heap(ctxt, buf.len());
        handle.length = buf.len();

        unsafe {
            let data = handle.data() as *mut u8;

            // copy buffer content into Str
            ptr::copy_nonoverlapping(buf.as_ptr(), data, buf.len());

            // string should end with 0 for C compatibility
            *(data.offset(buf.len() as isize)) = 0;
        }

        handle
    }

    pub fn concat(ctxt: &SemContext, lhs: Handle<Str>, rhs: Handle<Str>) -> Handle<Str> {
        let len = lhs.len() + rhs.len();
        let mut handle = str_alloc_heap(ctxt, len);

        unsafe {
            handle.length = len;

            ptr::copy_nonoverlapping(lhs.data(), handle.data() as *mut u8, lhs.len());
            ptr::copy_nonoverlapping(rhs.data(),
                                     handle.data().offset(lhs.len() as isize) as *mut u8,
                                     rhs.len());

            *(handle.data().offset(len as isize) as *mut u8) = 0;
        }

        handle
    }

    // duplicate string into a new object
    pub fn dup(&self, ctxt: &SemContext) -> Handle<Str> {
        let len = self.len();
        let mut handle = str_alloc_heap(ctxt, len);

        unsafe {
            handle.length = len;

            ptr::copy_nonoverlapping(self.data(), handle.data() as *mut u8, len);
            *(handle.data().offset(len as isize) as *mut u8) = 0;
        }

        handle
    }
}

fn str_alloc_heap(ctxt: &SemContext, len: usize) -> Handle<Str> {
    str_alloc(ctxt,
              len,
              |ctxt, size| ctxt.gc.alloc(ctxt, size) as *const u8)
}

fn str_alloc_perm(ctxt: &SemContext, len: usize) -> Handle<Str> {
    str_alloc(ctxt, len, |ctxt, size| ctxt.gc.alloc_perm(size))
}

fn str_alloc<F>(ctxt: &SemContext, len: usize, alloc: F) -> Handle<Str>
    where F: FnOnce(&SemContext, usize) -> *const u8
{
    let size = Header::size() as usize     // Object header
                + mem::ptr_width() as usize // length field
                + len + 1; // string content

    let size = mem::align_usize(size, mem::ptr_width() as usize);
    let ptr = alloc(ctxt, size) as usize;

    let clsid = ctxt.primitive_classes.str_class;
    let cls = ctxt.classes[clsid].borrow();
    let vtable: *const VTable = &**cls.vtable.as_ref().unwrap();
    let mut handle: Handle<Str> = ptr.into();
    handle.header_mut().vtable = vtable as *mut VTable;

    handle
}

#[repr(C)]
pub struct Array<T: Copy> {
    header: Header,
    length: usize,
    data: u8,
    phantom: PhantomData<T>,
}

impl<T> Array<T>
    where T: Copy
{
    pub fn header(&self) -> &Header {
        &self.header
    }

    pub fn header_mut(&mut self) -> &mut Header {
        &mut self.header
    }

    pub fn len(&self) -> usize {
        self.length
    }

    pub fn data(&self) -> *const T {
        &self.data as *const u8 as *const T
    }

    pub fn data_mut(&mut self) -> *mut T {
        &self.data as *const u8 as *mut T
    }

    pub fn size(&self) -> usize {
        Header::size() as usize         // Object header
            + mem::ptr_width() as usize // length field
            + self.len() * std::mem::size_of::<T>() // array content
    }

    pub fn alloc(ctxt: &SemContext, len: usize, elem: T, clsid: ClassId) -> Handle<Array<T>> {
        let size = Header::size() as usize        // Object header
                   + mem::ptr_width() as usize    // length field
                   + len * std::mem::size_of::<T>(); // array content

        let ptr = ctxt.gc.alloc(ctxt, size) as usize;
        let cls = ctxt.classes[clsid].borrow();
        let vtable: *const VTable = &**cls.vtable.as_ref().unwrap();
        let mut handle: Handle<Array<T>> = ptr.into();
        handle.header_mut().vtable = vtable as *mut VTable;
        handle.length = len;

        for i in 0..handle.len() {
            unsafe {
                *handle.data_mut().offset(i as isize) = elem;
            }
        }

        handle
    }
}

pub fn offset_of_array_length() -> i32 {
    offset_of!(Array<i32>, length) as i32
}

pub fn offset_of_array_data() -> i32 {
    offset_of!(Array<i32>, data) as i32
}

pub type BoolArray = Array<bool>;
pub type ByteArray = Array<u8>;
pub type CharArray = Array<char>;
pub type IntArray = Array<i32>;
pub type LongArray = Array<i64>;
pub type FloatArray = Array<f32>;
pub type DoubleArray = Array<f64>;
pub type StrArray = Array<Handle<Str>>;

pub fn alloc(ctxt: &SemContext, clsid: ClassId) -> Handle<Obj> {
    let cls = ctxt.classes[clsid].borrow();

    let ptr = ctxt.gc.alloc(ctxt, cls.size as usize) as usize;
    let vtable: *const VTable = &**cls.vtable.as_ref().unwrap();
    let mut handle: Handle<Obj> = ptr.into();
    handle.header_mut().vtable = vtable as *mut VTable;

    handle
}
