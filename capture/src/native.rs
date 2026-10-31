//! Native `amaterasu.capture-vkms/1` capture table, v1.8. Only DMA-BUF
//! delivery is offered; the CPU frame callbacks report MEDIA_UNSUPPORTED.
//! Reads and presentation queries use the frame the stream's presentation
//! thread stored and never wait for the helper.
use crate::{
    backend::Backend,
    controller::{Controller, StreamGeometry},
};
use amaterasu_media::{
    CAPTURE_API_V1_5_SIZE, CAPTURE_PRESENTATION_INTERFACE_MINOR, CaptureApiV1_6, CaptureGeometry,
    DmaBufFrameSink, DmaBufLeaseSink, FrameSink, FreshFrameSinkV1_2, FreshFrameSinkV1_3,
    INTERFACE_MAJOR, MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_UNSUPPORTED,
    PresentationStatus,
};
use amaterasu_module_sdk::abi::{
    AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery, NativeServiceFactory,
    STATUS_INTERNAL_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK, Status,
};
use sdk_resource::{CaptureApiV1_7, CaptureApiV1_9, CaptureEventSink, HdrMetadata};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, TryLockError},
};

pub type Shared<B> = Arc<Mutex<Controller<B>>>;

pub struct Factory<B: Backend> {
    pub api: NativeServiceFactory,
    controller: Shared<B>,
    header: AbiHeader,
    require_lease: bool,
}

impl<B: Backend + 'static> Factory<B> {
    pub fn new(controller: Shared<B>) -> Box<Self> {
        let settings = controller
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let retained = settings.supports_retained_frames();
        let require_lease = settings.requires_lease();
        drop(settings);
        let mut factory = Box::new(Self {
            api: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context: std::ptr::null_mut(),
                bind: Some(bind::<B>),
            },
            controller,
            header: capture_header(retained),
            require_lease,
        });
        factory.api.context = (&mut *factory as *mut Self).cast();
        factory
    }
}

fn capture_header(retained: bool) -> AbiHeader {
    AbiHeader {
        size: if retained {
            size_of::<CaptureApiV1_9>() as u32
        } else {
            CAPTURE_API_V1_5_SIZE
        },
        major: INTERFACE_MAJOR,
        minor: if retained {
            9
        } else {
            CAPTURE_PRESENTATION_INTERFACE_MINOR
        },
    }
}

struct Binding<B: Backend> {
    api: CaptureApiV1_9,
    resources: Mutex<std::collections::HashMap<u64, resource::OfferState>>,
    controller: Shared<B>,
    caller: String,
    require_lease: bool,
}

impl<B: Backend> Binding<B> {
    fn new(
        controller: Shared<B>,
        caller: String,
        header: AbiHeader,
        require_lease: bool,
    ) -> Box<Self> {
        let mut binding = Box::new(Self {
            api: CaptureApiV1_9 {
                base: CaptureApiV1_7 {
                    base: CaptureApiV1_6 {
                        base: NativeInterface {
                            header,
                            context: std::ptr::null_mut(),
                            release: Some(release::<B>),
                        },
                        read_frame,
                        geometry: geometry::<B>,
                        read_frame_with_freshness,
                        read_frame_with_capture_timestamp,
                        read_dmabuf_frame: read_dmabuf_frame::<B>,
                        latest_presentation: latest_presentation::<B>,
                        read_dmabuf_frame_leased: read_dmabuf_frame_leased::<B>,
                    },
                    subscribe_events: unsupported_events,
                    unsubscribe_events,
                    resolve_swap: unsupported_swap,
                },
                enumerate_resources: resource::enumerate::<B>,
                select_resource: resource::select::<B>,
                read_resource_frame: resource::read::<B>,
                hdr_metadata: hdr_metadata::<B>,
            },
            resources: Mutex::new(std::collections::HashMap::new()),
            controller,
            caller,
            require_lease,
        });
        binding.api.base.base.base.context = (&mut *binding as *mut Self).cast();
        binding
    }

    fn reader(&self, stream: u64) -> Result<crate::controller::Reader, Status> {
        let controller = match self.controller.try_lock() {
            Ok(controller) => controller,
            Err(TryLockError::WouldBlock) => return Err(MEDIA_BUSY),
            Err(TryLockError::Poisoned(_)) => return Err(MEDIA_BACKEND_ERROR),
        };
        controller
            .reader(&self.caller, stream)
            .map_err(|_| MEDIA_CLOSED)
    }
}

fn supports_query(query: &NativeInterfaceQuery, header: AbiHeader) -> bool {
    query.header.supports::<NativeInterfaceQuery>()
        && query.interface_major == INTERFACE_MAJOR
        && query.min_minor <= header.minor
        && query.min_size <= header.size
}

unsafe extern "C" fn bind<B: Backend + 'static>(
    context: *mut c_void,
    caller: BorrowedBytes,
    query: *const NativeInterfaceQuery,
    output: *mut *const NativeInterface,
) -> Status {
    guarded(|| {
        if output.is_null()
            || !(output as usize).is_multiple_of(align_of::<*const NativeInterface>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker supplies an aligned writable output slot for this call.
        unsafe { output.write(std::ptr::null()) };
        if context.is_null()
            || query.is_null()
            || !(query as usize).is_multiple_of(align_of::<NativeInterfaceQuery>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains the initialized aligned query until return.
        let query = unsafe { &*query };
        // SAFETY: provider retains this stable boxed factory until all binds stop.
        let factory = unsafe { &*context.cast::<Factory<B>>() };
        if !supports_query(query, factory.header) {
            return MEDIA_UNSUPPORTED;
        }
        // SAFETY: authenticated immediate caller bytes last through this synchronous bind.
        let Some(caller) = (unsafe { caller.read() })
            .ok()
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .filter(|caller| !caller.is_empty() && caller.len() <= 128)
        else {
            return STATUS_INVALID_ARGUMENT;
        };
        let binding = Binding::new(
            factory.controller.clone(),
            caller.into(),
            factory.header,
            factory.require_lease,
        );
        let interface = &binding.api.base.base.base as *const NativeInterface;
        let _ = Box::into_raw(binding);
        // SAFETY: transfers one stable boxed table, uniquely released by the broker.
        unsafe { output.write(interface) };
        STATUS_OK
    })
}

unsafe extern "C" fn release<B: Backend>(context: *mut c_void) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if !context.is_null() {
            // SAFETY: broker uniquely releases the binding after native calls stop.
            unsafe { drop(Box::from_raw(context.cast::<Binding<B>>())) };
        }
    }));
}

/// This provider has no CPU frames.
unsafe extern "C" fn read_frame(_: *mut c_void, _: u64, _: FrameSink) -> Status {
    MEDIA_UNSUPPORTED
}

unsafe extern "C" fn read_frame_with_freshness(
    _: *mut c_void,
    _: u64,
    _: FreshFrameSinkV1_2,
) -> Status {
    MEDIA_UNSUPPORTED
}

unsafe extern "C" fn read_frame_with_capture_timestamp(
    _: *mut c_void,
    _: u64,
    _: FreshFrameSinkV1_3,
) -> Status {
    MEDIA_UNSUPPORTED
}

unsafe extern "C" fn geometry<B: Backend>(
    context: *mut c_void,
    stream: u64,
    output: *mut CaptureGeometry,
) -> Status {
    guarded(|| {
        if context.is_null()
            || output.is_null()
            || !(output as usize).is_multiple_of(align_of::<CaptureGeometry>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: binding context is retained by the broker through this call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        match binding.reader(stream) {
            Ok(reader) => match reader.geometry {
                StreamGeometry::Ready(geometry) => {
                    // SAFETY: the validated aligned output slot is writable for
                    // this synchronous native call and geometry is an owned copy.
                    unsafe { output.write(geometry) };
                    STATUS_OK
                }
                StreamGeometry::NotReady => MEDIA_BUSY,
                StreamGeometry::Unsupported => MEDIA_UNSUPPORTED,
            },
            Err(status) => status,
        }
    })
}

unsafe extern "C" fn read_dmabuf_frame<B: Backend>(
    context: *mut c_void,
    stream: u64,
    sink: DmaBufFrameSink,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains the native binding until the synchronous call ends.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        if binding.require_lease {
            return MEDIA_UNSUPPORTED;
        }
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        // Close races with this admission: reads admitted before close may
        // finish, and none start after it.
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        let lent = match frames.lend() {
            Ok(lent) => lent,
            Err(status) => return status,
        };
        // SAFETY: `lent` shares ownership of the stored frame, so every
        // descriptor in it stays open until `finish` drops it after the
        // callback returns, even if the presentation thread stores a newer
        // frame meanwhile. Only this stream's read gate is held while
        // foreign code runs; the sink must duplicate any descriptor it keeps.
        let status = unsafe { sink.send(lent.frame(), lent.freshness) };
        frames.finish(lent, status)
    })
}

unsafe extern "C" fn read_dmabuf_frame_leased<B: Backend>(
    context: *mut c_void,
    stream: u64,
    sink: DmaBufLeaseSink,
) -> Status {
    guarded(|| {
        if context.is_null() {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains this binding through the synchronous call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        let lent = match frames.lend() {
            Ok(lent) => lent,
            Err(status) => return status,
        };
        let lease = match lent.lease() {
            Ok(lease) => lease,
            Err(status) => return status,
        };
        // SAFETY: the lent Arc pins immutable private storage through callback.
        // A consumer retaining it must keep this native binding live until release.
        let status = unsafe { sink.send(&lease, lent.freshness) };
        frames.finish(lent, status)
    })
}

unsafe extern "C" fn latest_presentation<B: Backend>(
    context: *mut c_void,
    stream: u64,
    output: *mut PresentationStatus,
) -> Status {
    guarded(|| {
        if context.is_null()
            || output.is_null()
            || !(output as usize).is_multiple_of(align_of::<PresentationStatus>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: broker retains the native binding until the synchronous call ends.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let frames = match binding.reader(stream) {
            Ok(reader) => reader.frames,
            Err(status) => return status,
        };
        let Some(_admission) = frames.admission.enter() else {
            return MEDIA_CLOSED;
        };
        match frames.latest_presentation() {
            Ok(presentation) => {
                // SAFETY: the validated aligned output slot is writable for
                // this synchronous native call and holds plain data.
                unsafe { output.write(presentation) };
                STATUS_OK
            }
            Err(status) => status,
        }
    })
}

fn guarded(operation: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(operation)).unwrap_or(STATUS_INTERNAL_ERROR)
}

unsafe extern "C" fn unsupported_events(_: *mut c_void, _: u64, _: CaptureEventSink) -> Status {
    MEDIA_UNSUPPORTED
}
unsafe extern "C" fn unsubscribe_events(_: *mut c_void, _: u64) {}
unsafe extern "C" fn unsupported_swap(
    _: *mut c_void,
    _: u64,
    _: u64,
    _: u32,
    _: BorrowedBytes,
) -> Status {
    MEDIA_UNSUPPORTED
}

mod resource;
mod sdk_resource;
unsafe extern "C" fn hdr_metadata<B: Backend>(
    context: *mut c_void,
    stream: u64,
    output: *mut HdrMetadata,
) -> Status {
    guarded(|| {
        if context.is_null()
            || output.is_null()
            || !(output as usize).is_multiple_of(align_of::<HdrMetadata>())
        {
            return STATUS_INVALID_ARGUMENT;
        }
        // SAFETY: the broker retains this binding and the output slot through this call.
        let binding = unsafe { &*context.cast::<Binding<B>>() };
        let reader = match binding.reader(stream) {
            Ok(reader) => reader,
            Err(status) => return status,
        };
        let Some((color, fields)) = reader.frames.color_status() else {
            return amaterasu_media::MEDIA_PENDING;
        };
        if !matches!(color, 1 | 2) {
            return MEDIA_UNSUPPORTED;
        }
        let metadata = HdrMetadata {
            enabled: u32::from(color == 2),
            fields,
            reserved: 0,
        };
        unsafe { output.write(metadata) };
        STATUS_OK
    })
}

#[cfg(test)]
mod tests;
