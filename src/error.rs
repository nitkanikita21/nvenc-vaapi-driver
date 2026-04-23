//! Error translation between internal [`DriverError`] and libva [`VAStatus`].
//!
//! Every `extern "C"` vtable function must return a `VAStatus` integer defined
//! by `<va/va.h>`. Internally we work with typed `DriverError` values and
//! convert at the ABI boundary via [`DriverError::to_status`].
#![allow(dead_code)]

use thiserror::Error;
use va_sys as va;

pub type VAStatus = va::VAStatus;

/// Typed driver error, convertible to a libva `VAStatus` integer.
///
/// Each variant maps 1-to-1 to a `VA_STATUS_ERROR_*` constant from `<va/va.h>`.
/// The mapping is enforced by [`DriverError::to_status`] and validated in unit
/// tests so that accidental table drift is caught at compile-time.
#[derive(Debug, Error)]
pub enum DriverError {
    /// A vtable slot is populated with a stub that returns this until the real
    /// implementation is wired in. Maps to `VA_STATUS_ERROR_UNIMPLEMENTED`.
    #[error("unimplemented vtable entry")]
    Unimplemented,

    /// The `VAConfigID` passed by the client does not exist in the config pool.
    /// Maps to `VA_STATUS_ERROR_INVALID_CONFIG`.
    #[error("invalid config id")]
    InvalidConfig,

    /// The `VAContextID` does not exist in the context pool.
    /// Maps to `VA_STATUS_ERROR_INVALID_CONTEXT`.
    #[error("invalid context id")]
    InvalidContext,

    /// The `VASurfaceID` does not exist in the surface pool.
    /// Maps to `VA_STATUS_ERROR_INVALID_SURFACE`.
    #[error("invalid surface id")]
    InvalidSurface,

    /// The `VABufferID` does not exist in the buffer pool.
    /// Maps to `VA_STATUS_ERROR_INVALID_BUFFER`.
    #[error("invalid buffer id")]
    InvalidBuffer,

    /// The `VAImageID` does not exist in the image pool.
    /// Maps to `VA_STATUS_ERROR_INVALID_IMAGE`.
    #[error("invalid image id")]
    InvalidImage,

    /// A required pointer argument was null, a size was zero, or a numeric
    /// argument was out of range. Maps to `VA_STATUS_ERROR_INVALID_PARAMETER`.
    #[error("invalid parameter")]
    InvalidParameter,

    /// The requested `VAProfile` is not in the advertised set
    /// (`H264ConstrainedBaseline`, `H264Main`).
    /// Maps to `VA_STATUS_ERROR_UNSUPPORTED_PROFILE`.
    #[error("unsupported profile")]
    UnsupportedProfile,

    /// The requested `VAEntrypoint` is not `VAEntrypointEncSlice`.
    /// Maps to `VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT`.
    #[error("unsupported entrypoint")]
    UnsupportedEntrypoint,

    /// The requested runtime format is not `VA_RT_FORMAT_YUV420`.
    /// Maps to `VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT`.
    #[error("unsupported rt format")]
    UnsupportedRtFormat,

    /// The buffer type is not handled by this driver version.
    /// Maps to `VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE`.
    #[error("unsupported buffer type")]
    UnsupportedBufferType,

    /// The surface memory type (e.g. a non-DRM-PRIME memory type on a surface
    /// attribute) is not supported.
    /// Maps to `VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE`.
    #[error("unsupported memory type")]
    UnsupportedMemory,

    /// A `VAConfigAttrib` type was requested but is not exposed by this driver.
    /// Maps to `VA_STATUS_ERROR_ATTR_NOT_SUPPORTED`.
    #[error("attribute not supported")]
    AttrNotSupported,

    /// A heap allocation failed (e.g. `Vec::try_reserve_exact` returned OOM).
    /// Maps to `VA_STATUS_ERROR_ALLOCATION_FAILED`.
    #[error("resource allocation failed")]
    AllocFailed,

    /// An NVENC encode call returned an error. The `&'static str` payload
    /// is a short human-readable description for logging.
    /// Maps to `VA_STATUS_ERROR_ENCODING_ERROR`.
    #[error("encoding error: {0}")]
    Encoding(&'static str),

    /// A driver-internal operation failed for a reason not covered by the
    /// variants above. Maps to `VA_STATUS_ERROR_OPERATION_FAILED`.
    #[error("operation failed: {0}")]
    OperationFailed(&'static str),
}

impl DriverError {
    pub const fn to_status(&self) -> VAStatus {
        match self {
            DriverError::Unimplemented => va::VA_STATUS_ERROR_UNIMPLEMENTED as VAStatus,
            DriverError::InvalidConfig => va::VA_STATUS_ERROR_INVALID_CONFIG as VAStatus,
            DriverError::InvalidContext => va::VA_STATUS_ERROR_INVALID_CONTEXT as VAStatus,
            DriverError::InvalidSurface => va::VA_STATUS_ERROR_INVALID_SURFACE as VAStatus,
            DriverError::InvalidBuffer => va::VA_STATUS_ERROR_INVALID_BUFFER as VAStatus,
            DriverError::InvalidImage => va::VA_STATUS_ERROR_INVALID_IMAGE as VAStatus,
            DriverError::InvalidParameter => va::VA_STATUS_ERROR_INVALID_PARAMETER as VAStatus,
            DriverError::UnsupportedProfile => va::VA_STATUS_ERROR_UNSUPPORTED_PROFILE as VAStatus,
            DriverError::UnsupportedEntrypoint => {
                va::VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT as VAStatus
            }
            DriverError::UnsupportedRtFormat => {
                va::VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT as VAStatus
            }
            DriverError::UnsupportedBufferType => {
                va::VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE as VAStatus
            }
            DriverError::UnsupportedMemory => {
                va::VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE as VAStatus
            }
            DriverError::AttrNotSupported => va::VA_STATUS_ERROR_ATTR_NOT_SUPPORTED as VAStatus,
            DriverError::AllocFailed => va::VA_STATUS_ERROR_ALLOCATION_FAILED as VAStatus,
            DriverError::Encoding(_) => va::VA_STATUS_ERROR_ENCODING_ERROR as VAStatus,
            DriverError::OperationFailed(_) => va::VA_STATUS_ERROR_OPERATION_FAILED as VAStatus,
        }
    }
}

pub type DriverResult<T> = Result<T, DriverError>;

/// Convenience: VA_STATUS_SUCCESS constant in i32 form (it's defined as
/// `#define VA_STATUS_SUCCESS 0x00000000` in va.h).
pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_STATUS_UNKNOWN: VAStatus = va::VA_STATUS_ERROR_UNKNOWN as VAStatus;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_maps_to_its_libva_status() {
        // The table below is a defence-in-depth check: if anyone changes the
        // DriverError -> VAStatus mapping by accident, this test fails with a
        // precise variant name. Keep in sync with the impl.
        let cases: &[(DriverError, u32)] = &[
            (DriverError::Unimplemented,        va::VA_STATUS_ERROR_UNIMPLEMENTED),
            (DriverError::InvalidConfig,        va::VA_STATUS_ERROR_INVALID_CONFIG),
            (DriverError::InvalidContext,       va::VA_STATUS_ERROR_INVALID_CONTEXT),
            (DriverError::InvalidSurface,       va::VA_STATUS_ERROR_INVALID_SURFACE),
            (DriverError::InvalidBuffer,        va::VA_STATUS_ERROR_INVALID_BUFFER),
            (DriverError::InvalidImage,         va::VA_STATUS_ERROR_INVALID_IMAGE),
            (DriverError::InvalidParameter,     va::VA_STATUS_ERROR_INVALID_PARAMETER),
            (DriverError::UnsupportedProfile,   va::VA_STATUS_ERROR_UNSUPPORTED_PROFILE),
            (DriverError::UnsupportedEntrypoint, va::VA_STATUS_ERROR_UNSUPPORTED_ENTRYPOINT),
            (DriverError::UnsupportedRtFormat,  va::VA_STATUS_ERROR_UNSUPPORTED_RT_FORMAT),
            (DriverError::UnsupportedBufferType, va::VA_STATUS_ERROR_UNSUPPORTED_BUFFERTYPE),
            (DriverError::UnsupportedMemory,    va::VA_STATUS_ERROR_UNSUPPORTED_MEMORY_TYPE),
            (DriverError::AttrNotSupported,     va::VA_STATUS_ERROR_ATTR_NOT_SUPPORTED),
            (DriverError::AllocFailed,          va::VA_STATUS_ERROR_ALLOCATION_FAILED),
            (DriverError::Encoding("x"),        va::VA_STATUS_ERROR_ENCODING_ERROR),
            (DriverError::OperationFailed("x"), va::VA_STATUS_ERROR_OPERATION_FAILED),
        ];
        for (err, expected) in cases {
            assert_eq!(
                err.to_status(),
                *expected as VAStatus,
                "variant {:?} maps to unexpected VAStatus",
                err
            );
        }
    }

    #[test]
    fn every_error_status_is_nonzero_and_distinct_from_success() {
        // Any error variant must not collide with VA_STATUS_SUCCESS (0).
        let variants = [
            DriverError::Unimplemented,
            DriverError::InvalidConfig,
            DriverError::InvalidContext,
            DriverError::InvalidSurface,
            DriverError::InvalidBuffer,
            DriverError::InvalidImage,
            DriverError::InvalidParameter,
            DriverError::UnsupportedProfile,
            DriverError::UnsupportedEntrypoint,
            DriverError::UnsupportedRtFormat,
            DriverError::UnsupportedBufferType,
            DriverError::UnsupportedMemory,
            DriverError::AttrNotSupported,
            DriverError::AllocFailed,
            DriverError::Encoding("x"),
            DriverError::OperationFailed("x"),
        ];
        for v in &variants {
            assert_ne!(v.to_status(), VA_STATUS_SUCCESS, "variant {:?}", v);
        }
    }

    #[test]
    fn driver_result_ok_is_success() {
        let r: DriverResult<()> = Ok(());
        let status = match r {
            Ok(()) => VA_STATUS_SUCCESS,
            Err(e) => e.to_status(),
        };
        assert_eq!(status, 0);
    }

    #[test]
    fn va_status_unknown_is_libva_unknown() {
        assert_eq!(VA_STATUS_UNKNOWN, va::VA_STATUS_ERROR_UNKNOWN as VAStatus);
    }
}
