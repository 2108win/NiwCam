//! niwcam-phone: the phone side of the PC engine (Rust port of `src/engine`, see rust/MAP.md).
//!
//! - `discovery`: mDNS `_camstudio._tcp` (DnsServiceBrowse/Resolve), manual phones, the cable list
//!   (`Discovery.{h,cpp}`)
//! - `usb_transport` (also `usb`): adb `host:track-devices` and usbmuxd, tunnels (`UsbTransport.{h,cpp}`)
//! - `phone_link`: the TCP link, handshake, pairing, key store, sealed records, lease
//!   (`PhoneLink.{h,cpp}`)
//! - `connector`: the background connector thread (`Connector.{h,cpp}`)
//!
//! Unsafe code is limited to WinSock, DPAPI, the DNS service API and the adb launcher; each block
//! has a `// SAFETY:` note.
#![deny(unsafe_op_in_unsafe_fn)]

mod appdata;
pub mod connector;
pub mod discovery;
pub mod phone_link;
pub mod sock;
pub mod usb_transport;
mod util;

pub use appdata::app_data_dir;
pub use usb_transport as usb;
pub use util::tick_ms;
