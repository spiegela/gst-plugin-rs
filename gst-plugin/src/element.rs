// Copyright (C) 2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use std::ptr;
use std::mem;
use mopa;

use libc;

use glib_ffi;
use gobject_ffi;
use gst_ffi;

use glib;
use glib::translate::*;
use gst;
use gst::prelude::*;

use object::*;

pub trait ElementImpl<T: Element>
    : ObjectImpl<T> + mopa::Any + Send + Sync + 'static {
    fn change_state(&self, element: &T, transition: gst::StateChange) -> gst::StateChangeReturn {
        element.parent_change_state(transition)
    }

    fn request_new_pad(
        &self,
        _element: &T,
        _templ: &gst::PadTemplate,
        _name: Option<String>,
        _caps: Option<&gst::CapsRef>,
    ) -> Option<gst::Pad> {
        None
    }

    fn release_pad(&self, _element: &T, _pad: &gst::Pad) {}

    fn send_event(&self, element: &T, event: gst::Event) -> bool {
        element.parent_send_event(event)
    }

    fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
        element.parent_query(query)
    }

    fn set_context(&self, element: &T, context: &gst::Context) {
        element.parent_set_context(context)
    }
}

mopafy_object_impl!(Element, ElementImpl);

pub unsafe trait Element: IsA<gst::Element> + ObjectType {
    fn parent_change_state(&self, transition: gst::StateChange) -> gst::StateChangeReturn {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstElementClass;
            (*parent_klass)
                .change_state
                .map(|f| {
                    from_glib(f(self.to_glib_none().0, transition.to_glib()))
                })
                .unwrap_or(gst::StateChangeReturn::Success)
        }
    }

    fn parent_send_event(&self, event: gst::Event) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstElementClass;
            (*parent_klass)
                .send_event
                .map(|f| from_glib(f(self.to_glib_none().0, event.into_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_query(&self, query: &mut gst::QueryRef) -> bool {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstElementClass;
            (*parent_klass)
                .query
                .map(|f| from_glib(f(self.to_glib_none().0, query.as_mut_ptr())))
                .unwrap_or(false)
        }
    }

    fn parent_set_context(&self, context: &gst::Context) {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gst_ffi::GstElementClass;
            (*parent_klass)
                .set_context
                .map(|f| f(self.to_glib_none().0, context.to_glib_none().0))
                .unwrap_or(())
        }
    }
}

pub unsafe trait ElementClass<T: Element>
where
    T::ImplType: ElementImpl<T>,
{
    fn add_pad_template(&mut self, pad_template: gst::PadTemplate) {
        unsafe {
            gst_ffi::gst_element_class_add_pad_template(
                self as *const Self as *mut gst_ffi::GstElementClass,
                pad_template.to_glib_full(),
            );
        }
    }

    fn set_metadata(
        &mut self,
        long_name: &str,
        classification: &str,
        description: &str,
        author: &str,
    ) {
        unsafe {
            gst_ffi::gst_element_class_set_metadata(
                self as *const Self as *mut gst_ffi::GstElementClass,
                long_name.to_glib_none().0,
                classification.to_glib_none().0,
                description.to_glib_none().0,
                author.to_glib_none().0,
            );
        }
    }

    fn override_vfuncs(&mut self, _: &ClassInitToken) {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gst_ffi::GstElementClass);
            klass.change_state = Some(element_change_state::<T>);
            klass.request_new_pad = Some(element_request_new_pad::<T>);
            klass.release_pad = Some(element_release_pad::<T>);
            klass.send_event = Some(element_send_event::<T>);
            klass.query = Some(element_query::<T>);
            klass.set_context = Some(element_set_context::<T>);
        }
    }
}

glib_wrapper! {
    pub struct RsElement(Object<InstanceStruct<RsElement>>): [gst::Element => gst_ffi::GstElement,
                                                              gst::Object => gst_ffi::GstObject];

    match fn {
        get_type => || get_type::<RsElement>(),
    }
}

unsafe impl<T: IsA<gst::Element> + ObjectType> Element for T {}
pub type RsElementClass = ClassStruct<RsElement>;

// FIXME: Boilerplate
unsafe impl ElementClass<RsElement> for RsElementClass {}

#[macro_export]
macro_rules! box_element_impl(
    ($name:ident) => {
        box_object_impl!($name);

        impl<T: Element> ElementImpl<T> for Box<$name<T>> {
            fn change_state(
                &self,
                element: &T,
                transition: gst::StateChange,
            ) -> gst::StateChangeReturn {
                let imp: &$name<T> = self.as_ref();
                imp.change_state(element, transition)
            }

            fn request_new_pad(&self, element: &T, templ: &gst::PadTemplate, name: Option<String>, caps: Option<&gst::CapsRef>) -> Option<gst::Pad> {
                let imp: &$name<T> = self.as_ref();
                imp.request_new_pad(element, templ, name, caps)
            }

            fn release_pad(&self, element: &T, pad: &gst::Pad) {
                let imp: &$name<T> = self.as_ref();
                imp.release_pad(element, pad)
            }

            fn send_event(&self, element: &T, event: gst::Event) -> bool {
                let imp: &$name<T> = self.as_ref();
                imp.send_event(element, event)
            }

            fn query(&self, element: &T, query: &mut gst::QueryRef) -> bool {
                let imp: &$name<T> = self.as_ref();
                ElementImpl::query(imp, element, query)
            }

            fn set_context(&self, element: &T, context: &gst::Context) {
                let imp: &$name<T> = self.as_ref();
                imp.set_context(element, context)
            }
        }
    };
);

box_element_impl!(ElementImpl);

impl ObjectType for RsElement {
    const NAME: &'static str = "RsElement";
    type GlibType = gst_ffi::GstElement;
    type GlibClassType = gst_ffi::GstElementClass;
    type ImplType = Box<ElementImpl<Self>>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gst_ffi::gst_element_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut RsElementClass) {
        klass.override_vfuncs(token);
    }

    object_type_fns!();
}

unsafe extern "C" fn element_change_state<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    transition: gst_ffi::GstStateChange,
) -> gst_ffi::GstStateChangeReturn
where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, gst::StateChangeReturn::Failure, {
        imp.change_state(&wrap, from_glib(transition))
    }).to_glib()
}

unsafe extern "C" fn element_request_new_pad<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    templ: *mut gst_ffi::GstPadTemplate,
    name: *const libc::c_char,
    caps: *const gst_ffi::GstCaps,
) -> *mut gst_ffi::GstPad
where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let caps = if caps.is_null() {
        None
    } else {
        Some(gst::CapsRef::from_ptr(caps))
    };

    panic_to_error!(&wrap, &element.panicked, None, {
        imp.request_new_pad(&wrap, &from_glib_borrow(templ), from_glib_none(name), caps)
    }).to_glib_full()
}

unsafe extern "C" fn element_release_pad<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    pad: *mut gst_ffi::GstPad,
) where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, (), {
        imp.release_pad(&wrap, &from_glib_borrow(pad))
    })
}

unsafe extern "C" fn element_send_event<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    event: *mut gst_ffi::GstEvent,
) -> glib_ffi::gboolean
where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, false, {
        imp.send_event(&wrap, from_glib_full(event))
    }).to_glib()
}

unsafe extern "C" fn element_query<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    query: *mut gst_ffi::GstQuery,
) -> glib_ffi::gboolean
where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;
    let query = gst::QueryRef::from_mut_ptr(query);

    panic_to_error!(&wrap, &element.panicked, false, { imp.query(&wrap, query) }).to_glib()
}

unsafe extern "C" fn element_set_context<T: Element>(
    ptr: *mut gst_ffi::GstElement,
    context: *mut gst_ffi::GstContext,
) where
    T::ImplType: ElementImpl<T>,
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let element = &*(ptr as *mut InstanceStruct<T>);
    let wrap: T = from_glib_borrow(ptr as *mut InstanceStruct<T>);
    let imp = &*element.imp;

    panic_to_error!(&wrap, &element.panicked, (), {
        imp.set_context(&wrap, &from_glib_borrow(context))
    })
}
