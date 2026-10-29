//! Native `amaterasu.capture-vkms/1` capture table, v1.6. Only DMA-BUF
//! delivery is offered; the CPU frame callbacks report MEDIA_UNSUPPORTED.
//! Reads and presentation queries use the frame the stream's presentation
//! thread stored and never wait for the helper.
use crate::{
    backend::Backend,
    controller::{Controller, StreamGeometry},
};
use amaterasu_media::{
    CAPTURE_API_V1_6_SIZE, CAPTURE_LEASE_INTERFACE_MINOR, CaptureApiV1_6, CaptureGeometry,
    DmaBufFrameSink, DmaBufLeaseSink, FrameSink, FreshFrameSinkV1_2, FreshFrameSinkV1_3,
    INTERFACE_MAJOR, MEDIA_BACKEND_ERROR, MEDIA_BUSY, MEDIA_CLOSED, MEDIA_UNSUPPORTED,
    PresentationStatus,
};
use amaterasu_module_sdk::abi::{
    AbiHeader, BorrowedBytes, NativeInterface, NativeInterfaceQuery, NativeServiceFactory,
    STATUS_INTERNAL_ERROR, STATUS_INVALID_ARGUMENT, STATUS_OK, Status,
};
use std::{
    ffi::c_void,
    panic::{AssertUnwindSafe, catch_unwind},
    sync::{Arc, Mutex, TryLockError},
};

pub type Shared<B> = Arc<Mutex<Controller<B>>>;

pub struct Factory<B: Backend> {
    pub api: NativeServiceFactory,
    controller: Shared<B>,
}

impl<B: Backend + 'static> Factory<B> {
    pub fn new(controller: Shared<B>) -> Box<Self> {
        let mut factory = Box::new(Self {
            api: NativeServiceFactory {
                header: AbiHeader::of::<NativeServiceFactory>(),
                context: std::ptr::null_mut(),
                bind: Some(bind::<B>),
            },
            controller,
        });
        factory.api.context = (&mut *factory as *mut Self).cast();
        factory
    }
}

struct Binding<B: Backend> {
    api: CaptureApiV1_6,
    controller: Shared<B>,
    caller: String,
}

impl<B: Backend> Binding<B> {
    fn new(controller: Shared<B>, caller: String) -> Box<Self> {
        let mut binding = Box::new(Self {
            api: CaptureApiV1_6 {
                base: NativeInterface {
                    header: AbiHeader {
                        size: CAPTURE_API_V1_6_SIZE,
                        major: INTERFACE_MAJOR,
                        minor: CAPTURE_LEASE_INTERFACE_MINOR,
                    },
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
            controller,
            caller,
        });
        binding.api.base.context = (&mut *binding as *mut Self).cast();
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

fn supports_query(query: &NativeInterfaceQuery) -> bool {
    query.header.supports::<NativeInterfaceQuery>()
        && query.interface_major == INTERFACE_MAJOR
        && query.min_minor <= CAPTURE_LEASE_INTERFACE_MINOR
        && query.min_size <= CAPTURE_API_V1_6_SIZE
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
        if !supports_query(query) {
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
        // SAFETY: provider retains this stable boxed factory until all binds stop.
        let factory = unsafe { &*context.cast::<Factory<B>>() };
        let binding = Binding::new(factory.controller.clone(), caller.into());
        let interface = &binding.api.base as *const NativeInterface;
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

#[cfg(test)]
mod tests;
