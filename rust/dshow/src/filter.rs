//! The filter object and its enumerators. Ports `SourceFilter`, `PinEnumerator` and `MediaTypeEnumerator`
//! of `src/dshow/SourceFilter.{h,cpp}`.

use crate::com::{
    IBaseFilterX, IBaseFilterX_Impl, IEnumMediaTypesX, IEnumMediaTypesX_Impl, IEnumPinsX,
    IEnumPinsX_Impl, IMediaFilterX_Impl, IPersistX_Impl,
};
use crate::media_type::{alloc_media_type, copy_media_type_fields, free_media_type_fields};
use crate::pin::{OutputPin, PIN_NAME};
use crate::{guard, lock, FilterShared, CLSID_CAMSTUDIO_DSHOW};
use core::ffi::c_void;
use core::ptr::{self, null_mut};
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering::SeqCst};
use std::sync::{Arc, Mutex};
use windows::core::{implement, ComObject, Interface, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Foundation::{E_FAIL, E_POINTER, S_FALSE, S_OK};
use windows::Win32::Media::DirectShow::{
    IFilterGraph, IPin, State_Paused, State_Running, State_Stopped, FILTER_INFO, FILTER_STATE,
    VFW_E_NOT_FOUND, VFW_S_CANT_CUE,
};
use windows::Win32::Media::IReferenceClock;
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;

// ---- PinEnumerator ----------------------------------------------------------------------------

#[implement(IEnumPinsX, Agile = false)]
struct PinEnumerator {
    pin: IPin,
    index: AtomicU32,
}

unsafe fn new_pin_enumerator(pin: &IPin, index: u32, out: *mut *mut c_void) -> HRESULT {
    let e = ComObject::new(PinEnumerator {
        pin: pin.clone(),
        index: AtomicU32::new(index),
    });
    // SAFETY: out is writable (checked by the callers); into_raw hands the AddRef to the caller.
    unsafe { *out = e.to_interface::<IEnumPinsX>().into_raw() };
    S_OK
}

// SAFETY (impl): COM passes null or valid pointers, per the interface contract; `guard` stops panics.
impl IEnumPinsX_Impl for PinEnumerator_Impl {
    unsafe fn Next(&self, count: u32, pins: *mut *mut c_void, fetched: *mut u32) -> HRESULT {
        guard(|| {
            if pins.is_null() || (count > 1 && fetched.is_null()) {
                return E_POINTER;
            }
            let mut n = 0u32;
            if count != 0 && self.index.load(SeqCst) == 0 {
                // SAFETY: pins has room for at least one pointer (count >= 1).
                unsafe { *pins = self.pin.clone().into_raw() };
                self.index.store(1, SeqCst);
                n = 1;
            }
            if !fetched.is_null() {
                // SAFETY: non-null, writable.
                unsafe { *fetched = n };
            }
            if n == count {
                S_OK
            } else {
                S_FALSE
            }
        })
    }
    unsafe fn Skip(&self, count: u32) -> HRESULT {
        let i = self.index.fetch_add(count, SeqCst).wrapping_add(count);
        if i > 1 {
            S_FALSE
        } else {
            S_OK
        }
    }
    unsafe fn Reset(&self) -> HRESULT {
        self.index.store(0, SeqCst);
        S_OK
    }
    unsafe fn Clone(&self, out: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if out.is_null() {
                return E_POINTER;
            }
            // SAFETY: out is non-null and writable (checked above).
            unsafe { new_pin_enumerator(&self.pin, self.index.load(SeqCst), out) }
        })
    }
}

// ---- MediaTypeEnumerator ----------------------------------------------------------------------

#[implement(IEnumMediaTypesX, Agile = false)]
struct MediaTypeEnumerator {
    types: Vec<AM_MEDIA_TYPE>,
    index: AtomicUsize,
}

impl Drop for MediaTypeEnumerator {
    fn drop(&mut self) {
        for t in &mut self.types {
            // SAFETY: each type was built or copied with a CoTaskMem format block.
            unsafe { free_media_type_fields(t) };
        }
    }
}

/// Makes an `IEnumMediaTypes` over `types` (it owns them) and writes it to `out`.
///
/// # Safety
/// `out` must be valid for writing a pointer.
pub unsafe fn new_media_type_enumerator(
    types: Vec<AM_MEDIA_TYPE>,
    index: usize,
    out: *mut *mut c_void,
) -> HRESULT {
    let e = ComObject::new(MediaTypeEnumerator {
        types,
        index: AtomicUsize::new(index),
    });
    // SAFETY: out is writable (checked by the callers); into_raw hands the AddRef to the caller.
    unsafe { *out = e.to_interface::<IEnumMediaTypesX>().into_raw() };
    S_OK
}

// SAFETY (impl): as above.
impl IEnumMediaTypesX_Impl for MediaTypeEnumerator_Impl {
    unsafe fn Next(&self, count: u32, out: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT {
        guard(|| {
            if out.is_null() || (count > 1 && fetched.is_null()) {
                return E_POINTER;
            }
            let mut n = 0u32;
            while n < count && self.index.load(SeqCst) < self.types.len() {
                let i = self.index.load(SeqCst);
                // SAFETY: out has room for `count` pointers; the type is valid.
                let p = unsafe { alloc_media_type(&self.types[i]) };
                // SAFETY: as above.
                unsafe { *out.add(n as usize) = p };
                if p.is_null() {
                    break;
                }
                n += 1;
                self.index.store(i + 1, SeqCst);
            }
            if !fetched.is_null() {
                // SAFETY: non-null, writable.
                unsafe { *fetched = n };
            }
            if n == count {
                S_OK
            } else {
                S_FALSE
            }
        })
    }
    unsafe fn Skip(&self, count: u32) -> HRESULT {
        let i = self.index.fetch_add(count as usize, SeqCst) + count as usize;
        if i > self.types.len() {
            S_FALSE
        } else {
            S_OK
        }
    }
    unsafe fn Reset(&self) -> HRESULT {
        self.index.store(0, SeqCst);
        S_OK
    }
    unsafe fn Clone(&self, out: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let mut copy = Vec::with_capacity(self.types.len());
            for t in &self.types {
                let mut c = AM_MEDIA_TYPE::default();
                // SAFETY: c is a valid destination, t a valid source.
                if unsafe { copy_media_type_fields(&mut c, t) }.is_err() {
                    break;
                }
                copy.push(c);
            }
            // SAFETY: out is non-null and writable (checked above).
            unsafe { new_media_type_enumerator(copy, self.index.load(SeqCst), out) }
        })
    }
}

// ---- SourceFilter -----------------------------------------------------------------------------

/// Under the filter's `m_lock`: the sync source and the name given to `JoinFilterGraph`.
struct FilterLocked {
    clock: Option<IReferenceClock>,
    name: [u16; 128],
}

/// `SourceFilter`: IBaseFilter (and IMediaFilter, IPersist through the chain).
#[implement(IBaseFilterX, Agile = false)]
pub struct SourceFilter {
    shared: Arc<FilterShared>,
    pin: ComObject<OutputPin>,
    locked: Mutex<FilterLocked>,
}

// SAFETY: the interface pointers inside are used from any thread, as the C++ does (ThreadingModel=Both);
// every mutable member is behind a mutex or an atomic.
unsafe impl Send for SourceFilter {}
// SAFETY: see above.
unsafe impl Sync for SourceFilter {}

impl SourceFilter {
    /// `RuntimeClassInitialize`: makes the pin (or fails with its error), and the filter around it.
    pub fn create() -> windows::core::Result<ComObject<SourceFilter>> {
        let pin = ComObject::new(OutputPin::new()?);
        let shared = Arc::new(FilterShared::default());
        let filter = ComObject::new(SourceFilter {
            shared: shared.clone(),
            pin: pin.clone(),
            locked: Mutex::new(FilterLocked {
                clock: None,
                name: [0; 128],
            }),
        });
        // The pin keeps a plain pointer to its filter (no reference back: no cycle).
        let iface: IBaseFilterX = filter.to_interface();
        pin.set_filter(iface.as_raw(), shared);
        Ok(filter)
    }

    fn state(&self) -> i32 {
        self.shared.state.load(SeqCst)
    }
}

impl Drop for SourceFilter {
    fn drop(&mut self) {
        self.pin.inner.orphan();
    }
}

impl SourceFilter_Impl {
    fn pin_interface(&self) -> Option<IPin> {
        self.pin
            .to_interface::<crate::com::IPinX>()
            .cast::<IPin>()
            .ok()
    }
}

// SAFETY (all methods of the impls below): COM passes null or valid pointers, per the interface contract;
// each method is wrapped in `guard` where it can run real code, so no panic crosses the boundary.
impl IPersistX_Impl for SourceFilter_Impl {
    unsafe fn GetClassID(&self, id: *mut GUID) -> HRESULT {
        if id.is_null() {
            return E_POINTER;
        }
        // SAFETY: non-null, writable.
        unsafe { *id = CLSID_CAMSTUDIO_DSHOW };
        S_OK
    }
}

impl IMediaFilterX_Impl for SourceFilter_Impl {
    unsafe fn Stop(&self) -> HRESULT {
        guard(|| {
            let _g = lock(&self.locked);
            if self.state() != State_Stopped.0 {
                self.pin.inner.inactive();
                self.shared.state.store(State_Stopped.0, SeqCst);
            }
            S_OK
        })
    }
    unsafe fn Pause(&self) -> HRESULT {
        guard(|| {
            let _g = lock(&self.locked);
            if self.state() == State_Stopped.0 && self.pin.inner.connected() {
                let hr = self.pin.inner.activate();
                if hr.is_err() {
                    return hr;
                }
            }
            self.shared.state.store(State_Paused.0, SeqCst);
            S_OK
        })
    }
    unsafe fn Run(&self, start: i64) -> HRESULT {
        guard(|| {
            let g = lock(&self.locked);
            if self.state() == State_Stopped.0 && self.pin.inner.connected() {
                let hr = self.pin.inner.activate();
                if hr.is_err() {
                    return hr;
                }
            }
            self.pin.inner.run(start, g.clock.clone());
            self.shared.state.store(State_Running.0, SeqCst);
            S_OK
        })
    }
    unsafe fn GetState(&self, _timeout_ms: u32, state: *mut i32) -> HRESULT {
        if state.is_null() {
            return E_POINTER;
        }
        let s = self.state();
        // SAFETY: non-null, writable.
        unsafe { *state = s };
        // A live source: nothing to cue while paused, so the graph does not wait for a sample.
        if FILTER_STATE(s) == State_Paused {
            VFW_S_CANT_CUE
        } else {
            S_OK
        }
    }
    unsafe fn SetSyncSource(&self, clock: *mut c_void) -> HRESULT {
        guard(|| {
            let mut g = lock(&self.locked);
            // SAFETY: null or a valid IReferenceClock; `cloned` is the AddRef the filter keeps.
            g.clock = unsafe { IReferenceClock::from_raw_borrowed(&clock) }.cloned();
            S_OK
        })
    }
    unsafe fn GetSyncSource(&self, clock: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if clock.is_null() {
                return E_POINTER;
            }
            let g = lock(&self.locked);
            // SAFETY: non-null, writable; into_raw hands the AddRef to the caller (null when none).
            unsafe {
                *clock = match &g.clock {
                    Some(c) => c.clone().into_raw(),
                    None => null_mut(),
                };
            }
            S_OK
        })
    }
}

impl IBaseFilterX_Impl for SourceFilter_Impl {
    unsafe fn EnumPins(&self, out: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if out.is_null() {
                return E_POINTER;
            }
            let Some(pin) = self.pin_interface() else {
                return E_FAIL;
            };
            // SAFETY: out is non-null and writable (checked above).
            unsafe { new_pin_enumerator(&pin, 0, out) }
        })
    }
    unsafe fn FindPin(&self, id: PCWSTR, pin: *mut *mut c_void) -> HRESULT {
        guard(|| {
            if pin.is_null() {
                return E_POINTER;
            }
            // SAFETY: non-null, writable.
            unsafe { *pin = null_mut() };
            // SAFETY: id is null or a NUL-terminated string (contract).
            let matches = !id.is_null()
                && unsafe { id.as_wide() } == PIN_NAME.encode_utf16().collect::<Vec<u16>>();
            if !matches {
                return VFW_E_NOT_FOUND;
            }
            let Some(p) = self.pin_interface() else {
                return E_FAIL;
            };
            // SAFETY: as above; into_raw hands the AddRef to the caller.
            unsafe { *pin = p.into_raw() };
            S_OK
        })
    }
    unsafe fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> HRESULT {
        guard(|| {
            if info.is_null() {
                return E_POINTER;
            }
            let g = lock(&self.locked);
            let graph = self.shared.graph.load(SeqCst);
            // SAFETY: info is writable; the graph pointer is the one JoinFilterGraph stored (not counted, per the
            // DirectShow rules), valid while the filter is in the graph. Like wcscpy_s, the whole name is copied
            // (it is NUL terminated, at most 127 characters).
            unsafe {
                ptr::copy_nonoverlapping(
                    g.name.as_ptr(),
                    (*info).achName.as_mut_ptr(),
                    name_len(&g.name) + 1,
                );
                let gr = IFilterGraph::from_raw_borrowed(&graph).cloned();
                ptr::write(&raw mut (*info).pGraph, core::mem::ManuallyDrop::new(gr));
            }
            S_OK
        })
    }
    unsafe fn JoinFilterGraph(&self, graph: *mut c_void, name: PCWSTR) -> HRESULT {
        guard(|| {
            let mut g = lock(&self.locked);
            self.shared.graph.store(graph, SeqCst);
            if name.is_null() {
                g.name[0] = 0;
            } else {
                // wcsncpy_s(m_name, name, _TRUNCATE): as much as fits, then a NUL.
                // SAFETY: name is a NUL-terminated string (contract).
                let w = unsafe { name.as_wide() };
                let n = w.len().min(g.name.len() - 1);
                g.name[..n].copy_from_slice(&w[..n]);
                g.name[n] = 0;
            }
            S_OK
        })
    }
    unsafe fn QueryVendorInfo(&self, _info: *mut PWSTR) -> HRESULT {
        windows::Win32::Foundation::E_NOTIMPL
    }
}

fn name_len(name: &[u16; 128]) -> usize {
    name.iter().position(|&c| c == 0).unwrap_or(127)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_len_stops_at_nul() {
        let mut n = [0u16; 128];
        n[0] = b'a' as u16;
        n[1] = b'b' as u16;
        assert_eq!(name_len(&n), 2);
    }
}
