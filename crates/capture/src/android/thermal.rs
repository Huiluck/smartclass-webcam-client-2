//! Thermal pressure, and the encoder step-down it asks for.
//!
//! The protocol forbids a device from changing `resolution` or `fps` on its own — an operator
//! `switch_camera` is the only way those move — so the answer to heat is a lower encoder
//! bitrate, which this module measures and [`ThermalStatus::bitrate_percent`] turns into a
//! fraction of whatever bitrate the pipeline was configured with.
//!
//! # Two kinds of "not available"
//!
//! The thermal service is API 30 (`android/thermal.h`), while this client runs on API 24 and
//! up, and the symbols live in `libandroid.so`, which the loader binds at load time or not at
//! all. [`ThermalMonitor::start`] therefore resolves the entry points the way the platform
//! expects a client to when its own library may not have them: `dlopen` + `dlsym`, with
//! `None` — no monitoring, no step-down, nothing logged as a failure — being the answer on a
//! device that predates the service.
//!
//! The second kind is a device that has the service and refuses to answer: a polling thread
//! that gets an error from it reports [`ThermalStatus::Nominal`], because throttling on a
//! reading nobody produced would cost quality for no reason.
//!
//! # Why the library is never closed
//!
//! The handle `dlopen` returns is deliberately not kept and never passed to `dlclose`:
//! `libandroid.so` is mapped into every Android process for its whole life, so the call only
//! bumps a reference count, and `dlfcn.h` warns that `dlclose` on Android may unmap a library
//! other code still holds — its own advice is to leave the library open instead. A second
//! [`ThermalMonitor::start`] therefore costs one reference count and not a mapping.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::mem;

use tracing::debug;

/// Opaque `AThermalManager` (`android/thermal.h`).
#[repr(C)]
pub(crate) struct AThermalManager {
    _private: [u8; 0],
}

// ---------------------------------------------------------------------------------------
// `dlfcn.h`: how the thermal entry points are reached.
//
// `libandroid.so` is mapped into every Android process, but its thermal entry points exist
// only from API 30 on, and a linked symbol is bound when the loader maps the library: a
// `#[link(name = "android")] extern "C"` block here would make the whole client `.so` fail to
// load on API 24..29, which is a process that never starts rather than one that never
// throttles. `dlopen` + `dlsym` moves that decision to [`ThermalMonitor::start`], where the
// missing service is an ordinary `None`.
// ---------------------------------------------------------------------------------------

/// `RTLD_NOW` (`dlfcn.h`): resolve the library's undefined symbols before `dlopen` returns.
///
/// The header defines it as `0x00002`, and notes that Android uses `RTLD_NOW` for its own
/// loads whatever the caller asks for. The value is spelled out rather than leaning on that,
/// because the name is a C macro and does not survive into Rust.
const RTLD_NOW: c_int = 0x0000_0002;

/// The library the thermal entry points live in (`android/thermal.h`).
const LIBANDROID: &CStr = c"libandroid.so";

/// `AThermal_acquireManager`, the one entry point that has to exist for the rest to be usable.
const SYMBOL_ACQUIRE: &CStr = c"AThermal_acquireManager";
/// `AThermal_getCurrentThermalStatus`.
const SYMBOL_GET_STATUS: &CStr = c"AThermal_getCurrentThermalStatus";
/// `AThermal_releaseManager`.
const SYMBOL_RELEASE: &CStr = c"AThermal_releaseManager";

#[link(name = "dl")]
unsafe extern "C" {
    /// `void* dlopen(const char* filename, int flags)` (`dlfcn.h`).
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    /// `void* dlsym(void* handle, const char* symbol)` (`dlfcn.h`).
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
}

/// `AThermalManager* AThermal_acquireManager()` (`android/thermal.h`, API 30).
type AcquireManagerFn = unsafe extern "C" fn() -> *mut AThermalManager;

/// `AThermalStatus AThermal_getCurrentThermalStatus(AThermalManager*)` (`android/thermal.h`).
///
/// The enum comes back as the `c_int` to which C gives it on this ABI.
type GetStatusFn = unsafe extern "C" fn(*mut AThermalManager) -> c_int;

/// `void AThermal_releaseManager(AThermalManager*)` (`android/thermal.h`, API 30).
type ReleaseManagerFn = unsafe extern "C" fn(*mut AThermalManager);

// ---------------------------------------------------------------------------------------
// `AThermalStatus` (`android/thermal.h`).
//
// The numbers are the header's: the ordering is the promise that a higher status means more
// throttling, which is what lets [`ThermalMonitor::status`] map them onto the level's
// step-down without a table of its own.
// ---------------------------------------------------------------------------------------

/// `ATHERMAL_STATUS_ERROR`: the service could not produce a reading.
const ATHERMAL_STATUS_ERROR: c_int = -1;
/// `ATHERMAL_STATUS_NONE`: not under throttling.
const ATHERMAL_STATUS_NONE: c_int = 0;
/// `ATHERMAL_STATUS_LIGHT`: light throttling that a user does not notice.
const ATHERMAL_STATUS_LIGHT: c_int = 1;
/// `ATHERMAL_STATUS_MODERATE`.
const ATHERMAL_STATUS_MODERATE: c_int = 2;
/// `ATHERMAL_STATUS_SEVERE`: the platform itself is throttling.
const ATHERMAL_STATUS_SEVERE: c_int = 3;
/// `ATHERMAL_STATUS_CRITICAL`: the platform has done everything it can to save power.
const ATHERMAL_STATUS_CRITICAL: c_int = 4;
/// `ATHERMAL_STATUS_EMERGENCY`: components are shutting down.
const ATHERMAL_STATUS_EMERGENCY: c_int = 5;
/// `ATHERMAL_STATUS_SHUTDOWN`: a shutdown is imminent.
const ATHERMAL_STATUS_SHUTDOWN: c_int = 6;

/// How hot the platform says the device is.
///
/// The variants follow `ATHERMAL_STATUS_*`, minus `ATHERMAL_STATUS_ERROR`: a failed reading
/// is not a temperature, and the caller never sees one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThermalStatus {
    /// `ATHERMAL_STATUS_NONE`: no pressure.
    Nominal,
    /// `ATHERMAL_STATUS_LIGHT`: the platform is watching, nothing is throttled yet.
    Light,
    /// `ATHERMAL_STATUS_MODERATE`: sustained load is starting to cost.
    Moderate,
    /// `ATHERMAL_STATUS_SEVERE`: the platform itself is throttling.
    Severe,
    /// `ATHERMAL_STATUS_CRITICAL`.
    Critical,
    /// `ATHERMAL_STATUS_EMERGENCY`.
    Emergency,
    /// `ATHERMAL_STATUS_SHUTDOWN`.
    Shutdown,
}

impl ThermalStatus {
    /// The platform's name for this level, for logs.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Nominal => "nominal",
            Self::Light => "light",
            Self::Moderate => "moderate",
            Self::Severe => "severe",
            Self::Critical => "critical",
            Self::Emergency => "emergency",
            Self::Shutdown => "shutdown",
        }
    }

    /// Percent of the configured bitrate this level still allows.
    ///
    /// The step-down is deliberately gentle at first: the only lever the protocol leaves a
    /// device is bitrate, so spending it in one jump on the first warm reading would cost
    /// picture quality on a device that is merely busy. The last two levels are the ones the
    /// platform reserves for "about to shut the camera down", where the stream surviving at
    /// any quality beats not surviving at all.
    pub(crate) fn bitrate_percent(self) -> u32 {
        match self {
            Self::Nominal | Self::Light => 100,
            Self::Moderate => 75,
            Self::Severe => 50,
            Self::Critical => 35,
            Self::Emergency | Self::Shutdown => 25,
        }
    }
}

/// The platform's thermal service, if this device has one.
pub(crate) struct ThermalMonitor {
    /// The manager handle, owned until `Drop`.
    manager: *mut AThermalManager,
    /// `AThermal_getCurrentThermalStatus`, resolved once beside the manager.
    get_status: GetStatusFn,
    /// `AThermal_releaseManager`, resolved once beside the manager.
    release: ReleaseManagerFn,
}

impl ThermalMonitor {
    /// Connects to the thermal service.
    ///
    /// Returns `None` on a device whose platform has no such service (below API 30) or whose
    /// library refuses to hand one over; both mean the same thing to the pipeline, which then
    /// never throttles.
    pub(crate) fn start() -> Option<Self> {
        // SAFETY: `LIBANDROID` is a NUL-terminated string with static lifetime, and `RTLD_NOW`
        // is a flag `dlfcn.h` defines; `dlopen` reads the name and the flag and nothing else.
        let library = unsafe { dlopen(LIBANDROID.as_ptr(), RTLD_NOW) };
        if library.is_null() {
            debug!("this device has no libandroid.so to resolve; thermal step-down is off");
            return None;
        }

        // SAFETY: `library` is the non-null handle `dlopen` just returned and was never closed,
        // and each name is a NUL-terminated string with static lifetime; `dlsym` reads the name
        // and returns the symbol's address, or null when this platform has no such symbol.
        let acquire = unsafe { dlsym(library, SYMBOL_ACQUIRE.as_ptr()) };
        let get_status = unsafe { dlsym(library, SYMBOL_GET_STATUS.as_ptr()) };
        let release = unsafe { dlsym(library, SYMBOL_RELEASE.as_ptr()) };
        if acquire.is_null() || get_status.is_null() || release.is_null() {
            debug!("this platform has no thermal service (it arrived in API 30); step-down is off");
            return None;
        }

        // SAFETY: `acquire` is the address `dlsym` resolved for `AThermal_acquireManager`, and
        // `android/thermal.h` declares that symbol `AThermalManager* AThermal_acquireManager()`
        // — no arguments, one pointer out — which is `AcquireManagerFn` exactly.
        let acquire = unsafe { mem::transmute::<*mut c_void, AcquireManagerFn>(acquire) };
        // SAFETY: `get_status` is the address `dlsym` resolved for
        // `AThermal_getCurrentThermalStatus`, and `android/thermal.h` declares that symbol
        // `AThermalStatus AThermal_getCurrentThermalStatus(AThermalManager*)` — one pointer in,
        // an `int`-represented enum out — which is `GetStatusFn` exactly.
        let get_status = unsafe { mem::transmute::<*mut c_void, GetStatusFn>(get_status) };
        // SAFETY: `release` is the address `dlsym` resolved for `AThermal_releaseManager`, and
        // `android/thermal.h` declares that symbol
        // `void AThermal_releaseManager(AThermalManager*)` — one pointer in, nothing out —
        // which is `ReleaseManagerFn` exactly.
        let release = unsafe { mem::transmute::<*mut c_void, ReleaseManagerFn>(release) };

        // SAFETY: `acquire` is `AThermal_acquireManager` as declared above: it takes no
        // arguments, and returns a manager owned by the caller or null when the service cannot
        // hand one over.
        let manager = unsafe { acquire() };
        if manager.is_null() {
            debug!("the thermal service would not hand out a manager; step-down is off");
            return None;
        }

        Some(Self {
            manager,
            get_status,
            release,
        })
    }

    /// Reads the current thermal status.
    ///
    /// A reading the platform refuses to give comes back as [`ThermalStatus::Nominal`], which
    /// leaves the encoder at its configured bitrate.
    pub(crate) fn status(&self) -> ThermalStatus {
        // SAFETY: `self.manager` came from `AThermal_acquireManager` and has not been released
        // — `Drop` does that, once, on the same handle — and `self.get_status` is the
        // `AThermal_getCurrentThermalStatus` that header declares to take one non-null manager.
        let status = unsafe { (self.get_status)(self.manager) };

        match status {
            ATHERMAL_STATUS_NONE => ThermalStatus::Nominal,
            ATHERMAL_STATUS_LIGHT => ThermalStatus::Light,
            ATHERMAL_STATUS_MODERATE => ThermalStatus::Moderate,
            ATHERMAL_STATUS_SEVERE => ThermalStatus::Severe,
            ATHERMAL_STATUS_CRITICAL => ThermalStatus::Critical,
            ATHERMAL_STATUS_EMERGENCY => ThermalStatus::Emergency,
            ATHERMAL_STATUS_SHUTDOWN => ThermalStatus::Shutdown,
            // `ATHERMAL_STATUS_ERROR`: the service refused to answer, which is not a
            // temperature, so it must not cost picture quality.
            ATHERMAL_STATUS_ERROR => ThermalStatus::Nominal,
            // Anything this build's header does not define — a status a future platform adds,
            // say. Unreadable for the same reason, and answered the same way.
            _ => ThermalStatus::Nominal,
        }
    }
}

impl Drop for ThermalMonitor {
    fn drop(&mut self) {
        if self.manager.is_null() {
            return;
        }
        // SAFETY: `self.manager` came from `AThermal_acquireManager` and is released exactly
        // once, here — `status` only reads through it, and `Drop` runs once — so the call gets
        // the one non-null handle `android/thermal.h` declares it takes.
        unsafe { (self.release)(self.manager) };
    }
}

// SAFETY: the manager is acquired and released on the capture thread alone, and the platform's
// own entry point is documented as safe to call through one handle at a time. What moves with
// the handle is the manager pointer and the two entry points resolved beside it, and neither
// entry point carries any state of this crate's: the platform keeps its own.
unsafe impl Send for ThermalMonitor {}
