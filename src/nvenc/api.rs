//! Load only the NVENC functions used by the encoder, without link-time GPU
//! dependencies. The library outlives every cached function pointer.
use crate::error::DriverError;
use core::ffi::c_void;
use libloading::Library;
use nvidia_video_codec_sdk::sys::nvEncodeAPI as nv;
use std::sync::OnceLock;

type Status = nv::NVENCSTATUS;
type Result<T> = std::result::Result<T, &'static str>;

pub(super) struct EncodeApi {
    pub open_encode_session_ex: unsafe extern "C" fn(
        *mut nv::NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS,
        *mut *mut c_void,
    ) -> Status,
    pub get_encode_preset_config_ex: unsafe extern "C" fn(
        *mut c_void,
        nv::GUID,
        nv::GUID,
        nv::NV_ENC_TUNING_INFO,
        *mut nv::NV_ENC_PRESET_CONFIG,
    ) -> Status,
    pub initialize_encoder:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_INITIALIZE_PARAMS) -> Status,
    pub reconfigure_encoder:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_RECONFIGURE_PARAMS) -> Status,
    pub destroy_encoder: unsafe extern "C" fn(*mut c_void) -> Status,
    pub create_bitstream_buffer:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_CREATE_BITSTREAM_BUFFER) -> Status,
    pub destroy_bitstream_buffer:
        unsafe extern "C" fn(*mut c_void, nv::NV_ENC_OUTPUT_PTR) -> Status,
    pub register_resource:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_REGISTER_RESOURCE) -> Status,
    pub unregister_resource: unsafe extern "C" fn(*mut c_void, nv::NV_ENC_REGISTERED_PTR) -> Status,
    pub map_input_resource:
        unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_MAP_INPUT_RESOURCE) -> Status,
    pub unmap_input_resource: unsafe extern "C" fn(*mut c_void, nv::NV_ENC_INPUT_PTR) -> Status,
    pub encode_picture: unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_PIC_PARAMS) -> Status,
    pub lock_bitstream: unsafe extern "C" fn(*mut c_void, *mut nv::NV_ENC_LOCK_BITSTREAM) -> Status,
    pub unlock_bitstream: unsafe extern "C" fn(*mut c_void, nv::NV_ENC_OUTPUT_PTR) -> Status,
    _library: Library,
}

static API: OnceLock<Result<EncodeApi>> = OnceLock::new();

pub(super) fn get() -> std::result::Result<&'static EncodeApi, DriverError> {
    API.get_or_init(|| EncodeApi::load("libnvidia-encode.so.1"))
        .as_ref()
        .map_err(|reason| DriverError::OperationFailed(reason))
}

impl EncodeApi {
    fn load(path: &str) -> Result<Self> {
        // SAFETY: load the platform NVENC runtime by its SONAME. No symbol is
        // called until its signature and the supported API version are checked.
        let library = unsafe { Library::new(path) }.map_err(|_| "NVENC runtime unavailable")?;
        type GetVersion = unsafe extern "C" fn(*mut u32) -> Status;
        type CreateInstance = unsafe extern "C" fn(*mut nv::NV_ENCODE_API_FUNCTION_LIST) -> Status;
        // SAFETY: these names and signatures are the NVENC bootstrap ABI.
        let version_fn =
            unsafe { library.get::<GetVersion>(b"NvEncodeAPIGetMaxSupportedVersion\0") }
                .map_err(|_| "NVENC version symbol unavailable")?;
        let create_fn = unsafe { library.get::<CreateInstance>(b"NvEncodeAPICreateInstance\0") }
            .map_err(|_| "NVENC instance symbol unavailable")?;
        let mut version = 0;
        // SAFETY: version is a writable local.
        if unsafe { version_fn(&mut version) } != Status::NV_ENC_SUCCESS {
            return Err("NVENC version query failed");
        }
        validate_version(version)?;
        let mut list = nv::NV_ENCODE_API_FUNCTION_LIST {
            version: nv::NV_ENCODE_API_FUNCTION_LIST_VER,
            ..Default::default()
        };
        // SAFETY: list is a correctly versioned writable ABI descriptor.
        if unsafe { create_fn(&mut list) } != Status::NV_ENC_SUCCESS {
            return Err("NVENC API creation failed");
        }
        Self::from_list(library, list)
    }

    fn from_list(library: Library, list: nv::NV_ENCODE_API_FUNCTION_LIST) -> Result<Self> {
        macro_rules! required {
            ($field:ident) => {
                list.$field
                    .ok_or(concat!("NVENC missing ", stringify!($field)))?
            };
        }
        Ok(Self {
            open_encode_session_ex: required!(nvEncOpenEncodeSessionEx),
            get_encode_preset_config_ex: required!(nvEncGetEncodePresetConfigEx),
            initialize_encoder: required!(nvEncInitializeEncoder),
            reconfigure_encoder: required!(nvEncReconfigureEncoder),
            destroy_encoder: required!(nvEncDestroyEncoder),
            create_bitstream_buffer: required!(nvEncCreateBitstreamBuffer),
            destroy_bitstream_buffer: required!(nvEncDestroyBitstreamBuffer),
            register_resource: required!(nvEncRegisterResource),
            unregister_resource: required!(nvEncUnregisterResource),
            map_input_resource: required!(nvEncMapInputResource),
            unmap_input_resource: required!(nvEncUnmapInputResource),
            encode_picture: required!(nvEncEncodePicture),
            lock_bitstream: required!(nvEncLockBitstream),
            unlock_bitstream: required!(nvEncUnlockBitstream),
            _library: library,
        })
    }
}

fn validate_version(version: u32) -> Result<()> {
    let required = (nv::NVENCAPI_MAJOR_VERSION << 4) | nv::NVENCAPI_MINOR_VERSION;
    if version < required {
        Err("NVENC driver API too old")
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn driver_version_must_cover_compiled_headers() {
        let required = (nv::NVENCAPI_MAJOR_VERSION << 4) | nv::NVENCAPI_MINOR_VERSION;
        assert!(validate_version(0).is_err());
        assert!(validate_version(required - 1).is_err());
        assert!(validate_version(required).is_ok());
        assert!(validate_version(required + 16).is_ok());
    }

    #[test]
    fn missing_library_and_symbols_are_errors_without_panicking() {
        assert!(EncodeApi::load("/nonexistent/nvenc-runtime.so").is_err());
        assert!(EncodeApi::load("libc.so.6").is_err());
    }

    #[test]
    fn incomplete_function_table_is_rejected() {
        // SAFETY: libc is used only as a library lifetime token in this test.
        let library = unsafe { Library::new("libc.so.6") }.unwrap();
        let result = EncodeApi::from_list(library, Default::default());
        assert!(matches!(
            result,
            Err("NVENC missing nvEncOpenEncodeSessionEx")
        ));
    }
}
