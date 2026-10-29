//! Opening and identifying a vibeshine_drm primary node without privilege.
//!
//! Both the helper and the capture module use this. Neither ever keeps DRM
//! master: the kernel hands master to the first opener of a node nobody owns,
//! and keeping it would stop the compositor from acquiring the device, so it
//! is released immediately. No other DRM state is changed.
use std::{
    ffi::CString,
    io,
    mem::{size_of, zeroed},
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub const DRM_MAJOR: u32 = 226;
pub const VIBESHINE_DRIVER_NAME: &[u8] = b"vibeshine_drm";
/// Largest Linux minor number (MINORBITS = 20).
pub const MAX_MINOR: u32 = (1 << 20) - 1;

#[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "x86",
    target_arch = "aarch64",
    target_arch = "arm",
    target_arch = "riscv64",
    target_arch = "loongarch64"
)))]
compile_error!("DRM ioctl numbers are encoded with the generic Linux layout");

pub const IOC_NONE: u32 = 0;
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;
const DRM_IOCTL_BASE: u32 = b'd' as u32;

/// Generic Linux `_IOC(direction, 'd', number, size)`.
pub const fn drm_ioctl(direction: u32, number: u32, size: usize) -> u32 {
    (direction << 30) | ((size as u32) << 16) | (DRM_IOCTL_BASE << 8) | number
}

/// Issues one ioctl whose argument is a single record updated in place.
///
/// # Safety
/// `request` must be an ioctl whose argument is exactly one `T` (or none when
/// `record` is null), and every pointer inside `T` must be valid for the
/// lengths stored beside it for the duration of the call.
pub unsafe fn ioctl_record<T>(device: &OwnedFd, request: u32, record: *mut T) -> io::Result<()> {
    loop {
        // SAFETY: the caller pairs the request number with its argument
        // type; the record pointer is valid for the duration of the call.
        let result = unsafe { libc::ioctl(device.as_raw_fd(), request as libc::Ioctl, record) };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// `struct drm_version` from the DRM uapi.
#[repr(C)]
struct DrmVersion {
    version_major: libc::c_int,
    version_minor: libc::c_int,
    version_patchlevel: libc::c_int,
    name_len: libc::size_t,
    name: *mut libc::c_char,
    date_len: libc::size_t,
    date: *mut libc::c_char,
    desc_len: libc::size_t,
    desc: *mut libc::c_char,
}

/// `struct drm_auth` from the DRM uapi.
#[repr(C)]
struct DrmAuth {
    magic: libc::c_uint,
}

pub const DRM_IOCTL_VERSION: u32 = drm_ioctl(IOC_READ | IOC_WRITE, 0x00, size_of::<DrmVersion>());
pub const DRM_IOCTL_AUTH_MAGIC: u32 = drm_ioctl(IOC_WRITE, 0x11, size_of::<DrmAuth>());
pub const DRM_IOCTL_DROP_MASTER: u32 = drm_ioctl(IOC_NONE, 0x1f, 0);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceNumber {
    pub major: u32,
    pub minor: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug)]
pub enum NodeError {
    /// The number is not a DRM primary node, or the opened file is not it.
    NotThisDevice,
    /// Opening, inspecting or releasing the node failed.
    Unavailable(io::Error),
    /// The node's DRM driver is not vibeshine_drm.
    WrongDriver,
}

/// Opens `/dev/dri/card<minor>` and proves it is exactly `number` and is
/// driven by vibeshine_drm. The returned descriptor is never DRM master.
pub fn open_vibeshine_node(number: DeviceNumber, access: Access) -> Result<OwnedFd, NodeError> {
    if number.major != DRM_MAJOR || number.minor > MAX_MINOR {
        return Err(NodeError::NotThisDevice);
    }
    let device = open_card_path(number.minor, access)?;
    verify_device_number(&device, number)?;
    if driver_name(&device)? != VIBESHINE_DRIVER_NAME {
        return Err(NodeError::WrongDriver);
    }
    release_unintended_master(&device)?;
    Ok(device)
}

fn open_card_path(minor: u32, access: Access) -> Result<OwnedFd, NodeError> {
    let path = CString::new(format!("/dev/dri/card{minor}")).expect("no interior NUL");
    let mode = match access {
        Access::ReadOnly => libc::O_RDONLY,
        Access::ReadWrite => libc::O_RDWR,
    };
    let flags = mode | libc::O_CLOEXEC | libc::O_NOCTTY | libc::O_NOFOLLOW;
    // SAFETY: `path` is a NUL-terminated string that outlives the call.
    let raw = unsafe { libc::open(path.as_ptr(), flags) };
    if raw < 0 {
        return Err(NodeError::Unavailable(io::Error::last_os_error()));
    }
    // SAFETY: open returned a new descriptor owned only by this value.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn verify_device_number(device: &OwnedFd, number: DeviceNumber) -> Result<(), NodeError> {
    // SAFETY: stat is plain data; zero is a valid initial value.
    let mut status: libc::stat = unsafe { zeroed() };
    // SAFETY: `status` is writable for one stat record.
    if unsafe { libc::fstat(device.as_raw_fd(), &mut status) } != 0 {
        return Err(NodeError::Unavailable(io::Error::last_os_error()));
    }
    let character_device = status.st_mode & libc::S_IFMT == libc::S_IFCHR;
    if !character_device || status.st_rdev != libc::makedev(number.major, number.minor) {
        return Err(NodeError::NotThisDevice);
    }
    Ok(())
}

/// The DRM driver name, truncated to 32 bytes.
fn driver_name(device: &OwnedFd) -> Result<Vec<u8>, NodeError> {
    let mut name = [0u8; 32];
    let mut version = DrmVersion {
        version_major: 0,
        version_minor: 0,
        version_patchlevel: 0,
        name_len: name.len(),
        name: name.as_mut_ptr().cast(),
        date_len: 0,
        date: std::ptr::null_mut(),
        desc_len: 0,
        desc: std::ptr::null_mut(),
    };
    // SAFETY: DRM_IOCTL_VERSION takes one drm_version; the kernel writes at
    // most `name_len` bytes into `name`, which lives through the call, and
    // nothing through the zero-length date and description buffers.
    unsafe { ioctl_record(device, DRM_IOCTL_VERSION, &mut version) }
        .map_err(NodeError::Unavailable)?;
    // The kernel reports the full length even when it truncated the copy; a
    // longer name never equals the expected one.
    if version.name_len > name.len() {
        return Ok(Vec::new());
    }
    Ok(name[..version.name_len].to_vec())
}

/// Master-only AUTH_MAGIC with the never-issued magic 0 answers EACCES on a
/// non-master descriptor and changes nothing. Otherwise drop the role.
fn release_unintended_master(device: &OwnedFd) -> Result<(), NodeError> {
    let mut auth = DrmAuth { magic: 0 };
    // SAFETY: DRM_IOCTL_AUTH_MAGIC takes one drm_auth that lives through the call.
    let probe = unsafe { ioctl_record(device, DRM_IOCTL_AUTH_MAGIC, &mut auth) };
    if matches!(&probe, Err(error) if error.raw_os_error() == Some(libc::EACCES)) {
        return Ok(());
    }
    // SAFETY: DROP_MASTER takes no argument; the null pointer is never read.
    match unsafe { ioctl_record::<u8>(device, DRM_IOCTL_DROP_MASTER, std::ptr::null_mut()) } {
        Ok(()) => Ok(()),
        // EINVAL: this descriptor was not master after all.
        Err(error) if error.raw_os_error() == Some(libc::EINVAL) => Ok(()),
        Err(error) => Err(NodeError::Unavailable(error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_pointer_width = "64")]
    fn ioctl_numbers_match_the_kernel_headers() {
        assert_eq!(DRM_IOCTL_VERSION, 0xC040_6400);
        assert_eq!(DRM_IOCTL_AUTH_MAGIC, 0x4004_6411);
        assert_eq!(DRM_IOCTL_DROP_MASTER, 0x0000_641F);
    }

    #[test]
    fn numbers_outside_drm_primary_nodes_are_rejected_before_opening() {
        for number in [
            DeviceNumber { major: 8, minor: 0 },
            DeviceNumber {
                major: DRM_MAJOR,
                minor: MAX_MINOR + 1,
            },
        ] {
            assert!(matches!(
                open_vibeshine_node(number, Access::ReadOnly),
                Err(NodeError::NotThisDevice)
            ));
        }
    }
}
