//! The DirectShow interfaces the filter implements, declared by hand with `#[interface]`.
//!
//! Why not the `IBaseFilter_Impl` / `IPin_Impl` traits of the `windows` crate: their generated shims turn
//! every method into `Result<T>`, which cannot say what the C++ says. `IMediaFilter::GetState` must write
//! the state *and* return `VFW_S_CANT_CUE` while paused; `Skip`, `Disconnect` and friends return `S_FALSE`;
//! `FindPin`, `ConnectedTo` and `EnumPins` set their out parameter to null before failing. Here every method
//! has the raw C signature (`HRESULT` and raw pointers), the IIDs and the vtable order are the system's
//! (checked against the `windows` crate types in the tests), so callers cannot tell the difference.

#![allow(
    clippy::too_many_arguments,
    reason = "IKsPropertySet::Get has 7 parameters plus this, as the C signature"
)]

use core::ffi::c_void;
use windows::core::{interface, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR, PWSTR};
use windows::Win32::Media::DirectShow::{Quality, FILTER_INFO, PIN_INFO};
use windows::Win32::Media::MediaFoundation::AM_MEDIA_TYPE;

// IPersist {0000010c-0000-0000-C000-000000000046}
#[interface("0000010c-0000-0000-C000-000000000046")]
pub unsafe trait IPersistX: IUnknown {
    fn GetClassID(&self, id: *mut GUID) -> HRESULT;
}

// IMediaFilter {56a86899-0ad4-11ce-b03a-0020af0ba770}
#[interface("56a86899-0ad4-11ce-b03a-0020af0ba770")]
pub unsafe trait IMediaFilterX: IPersistX {
    fn Stop(&self) -> HRESULT;
    fn Pause(&self) -> HRESULT;
    fn Run(&self, start: i64) -> HRESULT;
    fn GetState(&self, timeout_ms: u32, state: *mut i32) -> HRESULT;
    fn SetSyncSource(&self, clock: *mut c_void) -> HRESULT;
    fn GetSyncSource(&self, clock: *mut *mut c_void) -> HRESULT;
}

// IBaseFilter {56a86895-0ad4-11ce-b03a-0020af0ba770}
#[interface("56a86895-0ad4-11ce-b03a-0020af0ba770")]
pub unsafe trait IBaseFilterX: IMediaFilterX {
    fn EnumPins(&self, out: *mut *mut c_void) -> HRESULT;
    fn FindPin(&self, id: PCWSTR, pin: *mut *mut c_void) -> HRESULT;
    fn QueryFilterInfo(&self, info: *mut FILTER_INFO) -> HRESULT;
    fn JoinFilterGraph(&self, graph: *mut c_void, name: PCWSTR) -> HRESULT;
    fn QueryVendorInfo(&self, info: *mut PWSTR) -> HRESULT;
}

// IPin {56a86891-0ad4-11ce-b03a-0020af0ba770}
#[interface("56a86891-0ad4-11ce-b03a-0020af0ba770")]
pub unsafe trait IPinX: IUnknown {
    fn Connect(&self, receiver: *mut c_void, pmt: *const AM_MEDIA_TYPE) -> HRESULT;
    fn ReceiveConnection(&self, connector: *mut c_void, pmt: *const AM_MEDIA_TYPE) -> HRESULT;
    fn Disconnect(&self) -> HRESULT;
    fn ConnectedTo(&self, pin: *mut *mut c_void) -> HRESULT;
    fn ConnectionMediaType(&self, pmt: *mut AM_MEDIA_TYPE) -> HRESULT;
    fn QueryPinInfo(&self, info: *mut PIN_INFO) -> HRESULT;
    fn QueryDirection(&self, dir: *mut i32) -> HRESULT;
    fn QueryId(&self, id: *mut PWSTR) -> HRESULT;
    fn QueryAccept(&self, pmt: *const AM_MEDIA_TYPE) -> HRESULT;
    fn EnumMediaTypes(&self, out: *mut *mut c_void) -> HRESULT;
    fn QueryInternalConnections(&self, pins: *mut *mut c_void, count: *mut u32) -> HRESULT;
    fn EndOfStream(&self) -> HRESULT;
    fn BeginFlush(&self) -> HRESULT;
    fn EndFlush(&self) -> HRESULT;
    fn NewSegment(&self, start: i64, stop: i64, rate: f64) -> HRESULT;
}

// IAMStreamConfig {C6E13340-30AC-11d0-A18C-00A0C9118956}
#[interface("c6e13340-30ac-11d0-a18c-00a0c9118956")]
pub unsafe trait IAMStreamConfigX: IUnknown {
    fn SetFormat(&self, pmt: *mut AM_MEDIA_TYPE) -> HRESULT;
    fn GetFormat(&self, pmt: *mut *mut AM_MEDIA_TYPE) -> HRESULT;
    fn GetNumberOfCapabilities(&self, count: *mut i32, size: *mut i32) -> HRESULT;
    fn GetStreamCaps(&self, index: i32, pmt: *mut *mut AM_MEDIA_TYPE, caps: *mut u8) -> HRESULT;
}

// IKsPropertySet {31EFAC30-515C-11d0-A9AA-00AA0061BE93}
#[interface("31efac30-515c-11d0-a9aa-00aa0061be93")]
pub unsafe trait IKsPropertySetX: IUnknown {
    fn Set(
        &self,
        set: *const GUID,
        id: u32,
        instance: *mut c_void,
        instance_bytes: u32,
        data: *mut c_void,
        data_bytes: u32,
    ) -> HRESULT;
    fn Get(
        &self,
        set: *const GUID,
        id: u32,
        instance: *mut c_void,
        instance_bytes: u32,
        data: *mut c_void,
        data_bytes: u32,
        returned: *mut u32,
    ) -> HRESULT;
    fn QuerySupported(&self, set: *const GUID, id: u32, support: *mut u32) -> HRESULT;
}

// IQualityControl {56a868a5-0ad4-11ce-b03a-0020af0ba770}
#[interface("56a868a5-0ad4-11ce-b03a-0020af0ba770")]
pub unsafe trait IQualityControlX: IUnknown {
    fn Notify(&self, filter: *mut c_void, q: Quality) -> HRESULT;
    fn SetSink(&self, sink: *mut c_void) -> HRESULT;
}

// IEnumPins {56a86892-0ad4-11ce-b03a-0020af0ba770}
#[interface("56a86892-0ad4-11ce-b03a-0020af0ba770")]
pub unsafe trait IEnumPinsX: IUnknown {
    fn Next(&self, count: u32, pins: *mut *mut c_void, fetched: *mut u32) -> HRESULT;
    fn Skip(&self, count: u32) -> HRESULT;
    fn Reset(&self) -> HRESULT;
    fn Clone(&self, out: *mut *mut c_void) -> HRESULT;
}

// IEnumMediaTypes {89c31040-846b-11ce-97d3-00aa0055595a}
#[interface("89c31040-846b-11ce-97d3-00aa0055595a")]
pub unsafe trait IEnumMediaTypesX: IUnknown {
    fn Next(&self, count: u32, types: *mut *mut AM_MEDIA_TYPE, fetched: *mut u32) -> HRESULT;
    fn Skip(&self, count: u32) -> HRESULT;
    fn Reset(&self) -> HRESULT;
    fn Clone(&self, out: *mut *mut c_void) -> HRESULT;
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::Interface;
    use windows::Win32::Media::DirectShow::{
        IAMStreamConfig, IBaseFilter, IEnumMediaTypes, IEnumPins, IMediaFilter, IPin,
        IQualityControl,
    };
    use windows::Win32::Media::KernelStreaming::IKsPropertySet;
    use windows::Win32::System::Com::IPersist;

    #[test]
    fn iids_are_the_systems() {
        assert_eq!(IPersistX::IID, IPersist::IID);
        assert_eq!(IMediaFilterX::IID, IMediaFilter::IID);
        assert_eq!(IBaseFilterX::IID, IBaseFilter::IID);
        assert_eq!(IPinX::IID, IPin::IID);
        assert_eq!(IAMStreamConfigX::IID, IAMStreamConfig::IID);
        assert_eq!(IKsPropertySetX::IID, IKsPropertySet::IID);
        assert_eq!(IQualityControlX::IID, IQualityControl::IID);
        assert_eq!(IEnumPinsX::IID, IEnumPins::IID);
        assert_eq!(IEnumMediaTypesX::IID, IEnumMediaTypes::IID);
    }

    #[test]
    fn vtable_sizes_are_the_systems() {
        use core::mem::size_of;
        assert_eq!(
            size_of::<IBaseFilterX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IBaseFilter_Vtbl>()
        );
        assert_eq!(
            size_of::<IPinX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IPin_Vtbl>()
        );
        assert_eq!(
            size_of::<IAMStreamConfigX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IAMStreamConfig_Vtbl>()
        );
        assert_eq!(
            size_of::<IKsPropertySetX_Vtbl>(),
            size_of::<windows::Win32::Media::KernelStreaming::IKsPropertySet_Vtbl>()
        );
        assert_eq!(
            size_of::<IQualityControlX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IQualityControl_Vtbl>()
        );
        assert_eq!(
            size_of::<IEnumPinsX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IEnumPins_Vtbl>()
        );
        assert_eq!(
            size_of::<IEnumMediaTypesX_Vtbl>(),
            size_of::<windows::Win32::Media::DirectShow::IEnumMediaTypes_Vtbl>()
        );
    }
}
