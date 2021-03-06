/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use app_units::Au;
use canvas_traits::CanvasData;
use canvas_traits::CanvasImageData;
use dom::bindings::callback::CallbackContainer;
use dom::bindings::cell::DOMRefCell;
use dom::bindings::codegen::Bindings::PaintWorkletGlobalScopeBinding;
use dom::bindings::codegen::Bindings::PaintWorkletGlobalScopeBinding::PaintWorkletGlobalScopeMethods;
use dom::bindings::codegen::Bindings::VoidFunctionBinding::VoidFunction;
use dom::bindings::conversions::StringificationBehavior;
use dom::bindings::conversions::get_property;
use dom::bindings::conversions::get_property_jsval;
use dom::bindings::error::Error;
use dom::bindings::error::Fallible;
use dom::bindings::js::JS;
use dom::bindings::js::Root;
use dom::bindings::reflector::DomObject;
use dom::bindings::str::DOMString;
use dom::paintrenderingcontext2d::PaintRenderingContext2D;
use dom::paintsize::PaintSize;
use dom::workletglobalscope::WorkletGlobalScope;
use dom::workletglobalscope::WorkletGlobalScopeInit;
use dom_struct::dom_struct;
use euclid::Size2D;
use ipc_channel::ipc::IpcSender;
use ipc_channel::ipc::IpcSharedMemory;
use js::jsapi::Call;
use js::jsapi::Construct1;
use js::jsapi::HandleValue;
use js::jsapi::HandleValueArray;
use js::jsapi::Heap;
use js::jsapi::IsCallable;
use js::jsapi::IsConstructor;
use js::jsapi::JSAutoCompartment;
use js::jsapi::JS_ClearPendingException;
use js::jsapi::JS_IsExceptionPending;
use js::jsval::JSVal;
use js::jsval::ObjectValue;
use js::jsval::UndefinedValue;
use js::rust::Runtime;
use msg::constellation_msg::PipelineId;
use net_traits::image::base::Image;
use net_traits::image::base::PixelFormat;
use net_traits::image_cache::ImageCache;
use servo_atoms::Atom;
use servo_url::ServoUrl;
use std::cell::Cell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::ptr::null_mut;
use std::rc::Rc;
use std::sync::Arc;

/// https://drafts.css-houdini.org/css-paint-api/#paintworkletglobalscope
#[dom_struct]
pub struct PaintWorkletGlobalScope {
    /// The worklet global for this object
    worklet_global: WorkletGlobalScope,
    /// The image cache (used for generating invalid images).
    #[ignore_heap_size_of = "Arc"]
    image_cache: Arc<ImageCache>,
    /// https://drafts.css-houdini.org/css-paint-api/#paint-definitions
    paint_definitions: DOMRefCell<HashMap<Atom, Box<PaintDefinition>>>,
    /// https://drafts.css-houdini.org/css-paint-api/#paint-class-instances
    paint_class_instances: DOMRefCell<HashMap<Atom, Box<Heap<JSVal>>>>,
}

impl PaintWorkletGlobalScope {
    #[allow(unsafe_code)]
    pub fn new(runtime: &Runtime,
               pipeline_id: PipelineId,
               base_url: ServoUrl,
               init: &WorkletGlobalScopeInit)
               -> Root<PaintWorkletGlobalScope> {
        debug!("Creating paint worklet global scope for pipeline {}.", pipeline_id);
        let global = box PaintWorkletGlobalScope {
            worklet_global: WorkletGlobalScope::new_inherited(pipeline_id, base_url, init),
            image_cache: init.image_cache.clone(),
            paint_definitions: Default::default(),
            paint_class_instances: Default::default(),
        };
        unsafe { PaintWorkletGlobalScopeBinding::Wrap(runtime.cx(), global) }
    }

    pub fn perform_a_worklet_task(&self, task: PaintWorkletTask) {
        match task {
            PaintWorkletTask::DrawAPaintImage(name, size, sender) => self.draw_a_paint_image(name, size, sender),
        }
    }

    /// https://drafts.css-houdini.org/css-paint-api/#draw-a-paint-image
    fn draw_a_paint_image(&self,
                          name: Atom,
                          size: Size2D<Au>,
                          sender: IpcSender<CanvasData>)
    {
        // TODO: document paint definitions.
        self.invoke_a_paint_callback(name, size, sender);
    }

    /// https://drafts.css-houdini.org/css-paint-api/#invoke-a-paint-callback
    #[allow(unsafe_code)]
    fn invoke_a_paint_callback(&self,
                               name: Atom,
                               size: Size2D<Au>,
                               sender: IpcSender<CanvasData>)
    {
        let width = size.width.to_px().abs() as u32;
        let height = size.height.to_px().abs() as u32;
        debug!("Invoking a paint callback {}({},{}).", name, width, height);

        let cx = self.worklet_global.get_cx();
        let _ac = JSAutoCompartment::new(cx, self.worklet_global.reflector().get_jsobject().get());

        // TODO: Steps 1-2.1.
        // Step 2.2-5.1.
        rooted!(in(cx) let mut class_constructor = UndefinedValue());
        rooted!(in(cx) let mut paint_function = UndefinedValue());
        let rendering_context = match self.paint_definitions.borrow().get(&name) {
            None => {
                // Step 2.2.
                warn!("Drawing un-registered paint definition {}.", name);
                return self.send_invalid_image(size, sender);
            }
            Some(definition) => {
                // Step 5.1
                if !definition.constructor_valid_flag.get() {
                    debug!("Drawing invalid paint definition {}.", name);
                    return self.send_invalid_image(size, sender);
                }
                class_constructor.set(definition.class_constructor.get());
                paint_function.set(definition.paint_function.get());
                Root::from_ref(&*definition.context)
            }
        };

        // Steps 5.2-5.4
        // TODO: the spec requires calling the constructor now, but we might want to
        // prepopulate the paint instance in `RegisterPaint`, to avoid calling it in
        // the primary worklet thread.
        // https://github.com/servo/servo/issues/17377
        rooted!(in(cx) let mut paint_instance = UndefinedValue());
        match self.paint_class_instances.borrow_mut().entry(name.clone()) {
            Entry::Occupied(entry) => paint_instance.set(entry.get().get()),
            Entry::Vacant(entry) => {
                // Step 5.2-5.3
                let args = HandleValueArray::new();
                rooted!(in(cx) let mut result = null_mut());
                unsafe { Construct1(cx, class_constructor.handle(), &args, result.handle_mut()); }
                paint_instance.set(ObjectValue(result.get()));
                if unsafe { JS_IsExceptionPending(cx) } {
                    debug!("Paint constructor threw an exception {}.", name);
                    unsafe { JS_ClearPendingException(cx); }
                    self.paint_definitions.borrow_mut().get_mut(&name)
                        .expect("Vanishing paint definition.")
                        .constructor_valid_flag.set(false);
                    return self.send_invalid_image(size, sender);
                }
                // Step 5.4
                entry.insert(Box::new(Heap::default())).set(paint_instance.get());
            }
        };

        // TODO: Steps 6-7
        // Step 8
        // TODO: the spec requires creating a new paint rendering context each time,
        // this code recycles the same one.
        rendering_context.set_bitmap_dimensions(size);

        // Step 9
        let paint_size = PaintSize::new(self, size);

        // TODO: Step 10
        // Steps 11-12
        debug!("Invoking paint function {}.", name);
        let args_slice = [
            ObjectValue(rendering_context.reflector().get_jsobject().get()),
            ObjectValue(paint_size.reflector().get_jsobject().get()),
        ];
        let args = unsafe { HandleValueArray::from_rooted_slice(&args_slice) };
        rooted!(in(cx) let mut result = UndefinedValue());
        unsafe { Call(cx, paint_instance.handle(), paint_function.handle(), &args, result.handle_mut()); }

        // Step 13.
        if unsafe { JS_IsExceptionPending(cx) } {
            debug!("Paint function threw an exception {}.", name);
            unsafe { JS_ClearPendingException(cx); }
            return self.send_invalid_image(size, sender);
        }

        rendering_context.send_data(sender);
    }

    fn send_invalid_image(&self, size: Size2D<Au>, sender: IpcSender<CanvasData>) {
        debug!("Sending an invalid image.");
        let width = size.width.to_px().abs() as u32;
        let height = size.height.to_px().abs() as u32;
        let len = (width as usize) * (height as usize) * 4;
        let pixel = [0xFF, 0x00, 0x00, 0xFF];
        let bytes: Vec<u8> = pixel.iter().cloned().cycle().take(len).collect();
        let mut image = Image {
            width: width,
            height: height,
            format: PixelFormat::BGRA8,
            bytes: IpcSharedMemory::from_bytes(&*bytes),
            id: None,
        };
        self.image_cache.set_webrender_image_key(&mut image);
        let image_key = image.id.expect("Image cache should set image key.");
        let image_data = CanvasImageData { image_key: image_key };
        let canvas_data = CanvasData::Image(image_data);
        let _ = sender.send(canvas_data);
    }
}

impl PaintWorkletGlobalScopeMethods for PaintWorkletGlobalScope {
    #[allow(unsafe_code)]
    #[allow(unrooted_must_root)]
    /// https://drafts.css-houdini.org/css-paint-api/#dom-paintworkletglobalscope-registerpaint
    fn RegisterPaint(&self, name: DOMString, paint_ctor: Rc<VoidFunction>) -> Fallible<()> {
        let name = Atom::from(name);
        let cx = self.worklet_global.get_cx();
        rooted!(in(cx) let paint_obj = paint_ctor.callback_holder().get());
        rooted!(in(cx) let paint_val = ObjectValue(paint_obj.get()));

        debug!("Registering paint image name {}.", name);

        // Step 1.
        if name.is_empty() {
            return Err(Error::Type(String::from("Empty paint name."))) ;
        }

        // Step 2-3.
        if self.paint_definitions.borrow().contains_key(&name) {
            return Err(Error::InvalidModification);
        }

        // Step 4-6.
        debug!("Getting input properties.");
        let input_properties: Vec<DOMString> =
            unsafe { get_property(cx, paint_obj.handle(), "inputProperties", StringificationBehavior::Default) }?
            .unwrap_or_default();
        debug!("Got {:?}.", input_properties);

        // Step 7-9.
        debug!("Getting input arguments.");
        let input_arguments: Vec<DOMString> =
            unsafe { get_property(cx, paint_obj.handle(), "inputArguments", StringificationBehavior::Default) }?
            .unwrap_or_default();
        debug!("Got {:?}.", input_arguments);

        // TODO: Steps 10-11.

        // Steps 12-13.
        debug!("Getting alpha.");
        let alpha: bool =
            unsafe { get_property(cx, paint_obj.handle(), "alpha", ()) }?
            .unwrap_or(true);
        debug!("Got {:?}.", alpha);

        // Step 14
        if unsafe { !IsConstructor(paint_obj.get()) } {
            return Err(Error::Type(String::from("Not a constructor.")));
        }

        // Steps 15-16
        rooted!(in(cx) let mut prototype = UndefinedValue());
        unsafe { get_property_jsval(cx, paint_obj.handle(), "prototype", prototype.handle_mut())?; }
        if !prototype.is_object() {
            return Err(Error::Type(String::from("Prototype is not an object.")));
        }
        rooted!(in(cx) let prototype = prototype.to_object());

        // Steps 17-18
        rooted!(in(cx) let mut paint_function = UndefinedValue());
        unsafe { get_property_jsval(cx, prototype.handle(), "paint", paint_function.handle_mut())?; }
        if !paint_function.is_object() || unsafe { !IsCallable(paint_function.to_object()) } {
            return Err(Error::Type(String::from("Paint function is not callable.")));
        }

        // Step 19.
        let context = PaintRenderingContext2D::new(self);
        let definition = PaintDefinition::new(paint_val.handle(),
                                              paint_function.handle(),
                                              input_properties,
                                              alpha,
                                              &*context);

        // Step 20.
        debug!("Registering definition {}.", name);
        self.paint_definitions.borrow_mut().insert(name, definition);

        // TODO: Step 21.

        Ok(())
    }
}

/// Tasks which can be peformed by a paint worklet
pub enum PaintWorkletTask {
    DrawAPaintImage(Atom, Size2D<Au>, IpcSender<CanvasData>)
}

/// A paint definition
/// https://drafts.css-houdini.org/css-paint-api/#paint-definition
/// This type is dangerous, because it contains uboxed `Heap<JSVal>` values,
/// which can't be moved.
#[derive(JSTraceable, HeapSizeOf)]
#[must_root]
struct PaintDefinition {
    class_constructor: Heap<JSVal>,
    paint_function: Heap<JSVal>,
    constructor_valid_flag: Cell<bool>,
    input_properties: Vec<DOMString>,
    context_alpha_flag: bool,
    // TODO: the spec calls for fresh rendering contexts each time a paint image is drawn,
    // but to avoid having the primary worklet thread create a new renering context,
    // we recycle them.
    context: JS<PaintRenderingContext2D>,
}

impl PaintDefinition {
    fn new(class_constructor: HandleValue,
           paint_function: HandleValue,
           input_properties: Vec<DOMString>,
           alpha: bool,
           context: &PaintRenderingContext2D)
           -> Box<PaintDefinition>
    {
        let result = Box::new(PaintDefinition {
            class_constructor: Heap::default(),
            paint_function: Heap::default(),
            constructor_valid_flag: Cell::new(true),
            input_properties: input_properties,
            context_alpha_flag: alpha,
            context: JS::from_ref(context),
        });
        result.class_constructor.set(class_constructor.get());
        result.paint_function.set(paint_function.get());
        result
    }
}
