//! VAAPI config vtable entries: `vaQueryConfigProfiles`, `vaQueryConfigEntrypoints`,
//! `vaGetConfigAttributes`, `vaCreateConfig`, `vaDestroyConfig`,
//! `vaQueryConfigAttributes`.
//!
//! Supported profiles: `VAProfileH264ConstrainedBaseline`, `VAProfileH264Main`.
//! Supported entrypoints: `VAEntrypointEncSlice` only.
//! Supported RT format: `VA_RT_FORMAT_YUV420`.
//! Supported RC modes: `VA_RC_CBR | VA_RC_VBR | VA_RC_CQP`.

use super::{DriverState, guard, state_from};
use crate::error::{DriverError, VAStatus};
use crate::ids::{ConfigKey, ContainsKey, key_to_id};
use crate::driver::state::ConfigRec;
use core::ffi::c_int;
use va_sys as va;

const SUPPORTED_PROFILES: &[va::VAProfile] = &[
    va::VAProfileH264ConstrainedBaseline,
    va::VAProfileH264Main,
    va::VAProfileH264High,
];

const SUPPORTED_ENTRYPOINTS: &[va::VAEntrypoint] = &[va::VAEntrypointEncSlice];

pub unsafe extern "C" fn query_config_profiles(
    _ctx: va::VADriverContextP,
    profile_list: *mut va::VAProfile,
    num_profiles: *mut c_int,
) -> VAStatus {
    guard(|| {
        if profile_list.is_null() || num_profiles.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        // SAFETY: caller provides an array sized by ctx->max_profiles; we
        // write at most SUPPORTED_PROFILES.len() entries.
        unsafe {
            for (i, p) in SUPPORTED_PROFILES.iter().enumerate() {
                *profile_list.add(i) = *p;
            }
            *num_profiles = SUPPORTED_PROFILES.len() as c_int;
        }
        Ok(())
    })
}

pub unsafe extern "C" fn query_config_entrypoints(
    _ctx: va::VADriverContextP,
    profile: va::VAProfile,
    entrypoint_list: *mut va::VAEntrypoint,
    num_entrypoints: *mut c_int,
) -> VAStatus {
    guard(|| {
        if entrypoint_list.is_null() || num_entrypoints.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        if !SUPPORTED_PROFILES.contains(&profile) {
            return Err(DriverError::UnsupportedProfile);
        }
        // SAFETY: see above.
        unsafe {
            for (i, e) in SUPPORTED_ENTRYPOINTS.iter().enumerate() {
                *entrypoint_list.add(i) = *e;
            }
            *num_entrypoints = SUPPORTED_ENTRYPOINTS.len() as c_int;
        }
        Ok(())
    })
}

pub unsafe extern "C" fn get_config_attributes(
    _ctx: va::VADriverContextP,
    profile: va::VAProfile,
    entrypoint: va::VAEntrypoint,
    attrib_list: *mut va::VAConfigAttrib,
    num_attribs: c_int,
) -> VAStatus {
    guard(|| {
        if attrib_list.is_null() || num_attribs <= 0 {
            return Err(DriverError::InvalidParameter);
        }
        if !SUPPORTED_PROFILES.contains(&profile) {
            return Err(DriverError::UnsupportedProfile);
        }
        if !SUPPORTED_ENTRYPOINTS.contains(&entrypoint) {
            return Err(DriverError::UnsupportedEntrypoint);
        }
        // SAFETY: caller provides a valid array of `num_attribs` entries.
        let attrs = unsafe {
            core::slice::from_raw_parts_mut(attrib_list, num_attribs as usize)
        };
        for a in attrs {
            a.value = match a.type_ {
                va::VAConfigAttribRTFormat => va::VA_RT_FORMAT_YUV420,
                va::VAConfigAttribRateControl => {
                    va::VA_RC_CBR | va::VA_RC_VBR | va::VA_RC_CQP
                }
                va::VAConfigAttribEncPackedHeaders => {
                    va::VA_ENC_PACKED_HEADER_SEQUENCE
                        | va::VA_ENC_PACKED_HEADER_PICTURE
                        | va::VA_ENC_PACKED_HEADER_SLICE
                }
                va::VAConfigAttribEncMaxRefFrames => 1,
                va::VAConfigAttribEncInterlaced => va::VA_ENC_INTERLACED_NONE,
                va::VAConfigAttribEncMaxSlices => 1,
                va::VAConfigAttribEncSliceStructure => {
                    va::VA_ENC_SLICE_STRUCTURE_POWER_OF_TWO_ROWS
                }
                _ => va::VA_ATTRIB_NOT_SUPPORTED,
            };
        }
        Ok(())
    })
}

pub unsafe extern "C" fn create_config(
    ctx: va::VADriverContextP,
    profile: va::VAProfile,
    entrypoint: va::VAEntrypoint,
    attrib_list: *mut va::VAConfigAttrib,
    num_attribs: c_int,
    config_id: *mut va::VAConfigID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx validity delegated to libva.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        if config_id.is_null() {
            return Err(DriverError::InvalidParameter);
        }
        if !SUPPORTED_PROFILES.contains(&profile) {
            return Err(DriverError::UnsupportedProfile);
        }
        if !SUPPORTED_ENTRYPOINTS.contains(&entrypoint) {
            return Err(DriverError::UnsupportedEntrypoint);
        }
        let attrs = if attrib_list.is_null() || num_attribs <= 0 {
            &[][..]
        } else {
            // SAFETY: caller provides a valid array.
            unsafe { core::slice::from_raw_parts(attrib_list, num_attribs as usize) }
        };
        let mut rt_format = va::VA_RT_FORMAT_YUV420;
        let mut rc_mode = va::VA_RC_CBR;
        let mut packed = 0u32;
        for a in attrs {
            match a.type_ {
                va::VAConfigAttribRTFormat => {
                    if a.value & va::VA_RT_FORMAT_YUV420 == 0 {
                        return Err(DriverError::UnsupportedRtFormat);
                    }
                    rt_format = a.value;
                }
                va::VAConfigAttribRateControl => rc_mode = a.value,
                va::VAConfigAttribEncPackedHeaders => packed = a.value,
                _ => {}
            }
        }
        let rec = ConfigRec {
            profile,
            entrypoint,
            rt_format,
            rc_mode,
            packed_headers: packed,
        };
        let mut pools = state.pools.lock();
        let k: ConfigKey = pools.configs.insert(rec);
        // SAFETY: config_id non-null per check above.
        unsafe { *config_id = key_to_id(k) as va::VAConfigID };
        Ok(())
    })
}

pub unsafe extern "C" fn destroy_config(
    ctx: va::VADriverContextP,
    config_id: va::VAConfigID,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let mut pools = state.pools.lock();
        let k: ConfigKey = pools
            .configs
            .find_by_low_bits(config_id as u32)
            .ok_or(DriverError::InvalidConfig)?;
        pools.configs.remove(k);
        Ok(())
    })
}

/// Per-config capability summary reported by `vaQueryConfigAttributes`.
///
/// Chromium's `FillProfileInfo_Locked` in `vaapi_wrapper.cc` calls this after
/// `vaQuerySurfaceAttributes` to discover which runtime format bits the driver
/// supports for the just-created config. Its pass condition is
/// `attrib.value & (VA_RT_FORMAT_YUV420 | YUV420_10 | YUV422 | YUV444) != 0`;
/// if we report zero attributes, Chromium records "no supported internal
/// formats" and silently drops the profile. Every encode entrypoint therefore
/// needs at least one `VAConfigAttribRTFormat` attribute listed here.
fn config_attribs() -> [va::VAConfigAttrib; 6] {
    [
        va::VAConfigAttrib {
            type_: va::VAConfigAttribRTFormat,
            value: va::VA_RT_FORMAT_YUV420,
        },
        va::VAConfigAttrib {
            type_: va::VAConfigAttribRateControl,
            value: va::VA_RC_CBR | va::VA_RC_VBR | va::VA_RC_CQP,
        },
        va::VAConfigAttrib {
            type_: va::VAConfigAttribEncPackedHeaders,
            // SEQUENCE | PICTURE | SLICE = 0x7; same set we advertise from
            // vaGetConfigAttributes.
            value: 0x7,
        },
        va::VAConfigAttrib {
            type_: va::VAConfigAttribEncMaxRefFrames,
            // L0=1, L1=0 (one reference frame, no B-frames).
            value: 1,
        },
        va::VAConfigAttrib {
            type_: va::VAConfigAttribEncMaxSlices,
            value: 1,
        },
        va::VAConfigAttrib {
            type_: va::VAConfigAttribEncSliceStructure,
            // POWER_OF_TWO_ROWS — arbitrary slice layout is not supported by
            // NVENC, but Chromium only checks that *some* non-zero value is
            // reported.
            value: 4,
        },
    ]
}

pub unsafe extern "C" fn query_config_attributes(
    ctx: va::VADriverContextP,
    config_id: va::VAConfigID,
    profile: *mut va::VAProfile,
    entrypoint: *mut va::VAEntrypoint,
    attrib_list: *mut va::VAConfigAttrib,
    num_attribs: *mut c_int,
) -> VAStatus {
    guard(|| {
        // SAFETY: ctx valid.
        let state = unsafe { state_from(ctx) }.ok_or(DriverError::InvalidParameter)?;
        let pools = state.pools.lock();
        let k: ConfigKey = pools
            .configs
            .find_by_low_bits(config_id as u32)
            .ok_or(DriverError::InvalidConfig)?;
        let rec = &pools.configs[k];
        let attribs = config_attribs();
        // SAFETY: out pointers owned by libva client; `num_attribs` is required,
        // the rest may be null if the caller only wants a subset of fields.
        unsafe {
            if !profile.is_null() {
                *profile = rec.profile;
            }
            if !entrypoint.is_null() {
                *entrypoint = rec.entrypoint;
            }
            if !num_attribs.is_null() {
                *num_attribs = attribs.len() as c_int;
            }
            if !attrib_list.is_null() {
                core::ptr::copy_nonoverlapping(attribs.as_ptr(), attrib_list, attribs.len());
            }
        }
        Ok(())
    })
}

/// `find_key` by low bits for Configs (helper used by other modules).
pub fn find_config(state: &DriverState, id: va::VAConfigID) -> Option<ConfigKey> {
    state.pools.lock().configs.find_by_low_bits(id as u32)
}
