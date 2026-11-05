#[cfg_attr(feature = "nightly-rustdoc", doc(cfg(target_os = "windows")))]
#[cfg_attr(not(feature = "nightly-rustdoc"), cfg(target_os = "windows"))]
pub mod win32;

#[cfg(target_os = "windows")]
pub(crate) use win32 as imp;

#[cfg_attr(feature = "nightly-rustdoc", doc(cfg(target_os = "linux")))]
#[cfg_attr(not(feature = "nightly-rustdoc"), cfg(target_os = "linux"))]
pub mod linux;

#[cfg(target_os = "linux")]
pub(crate) use linux as imp;

// Also the iOS backend: it is only an event queue the app fills plus the surface size, with nothing
// from Android in it, so both phones share it under this name.
#[cfg_attr(not(feature = "nightly-rustdoc"), cfg(any(target_os = "android", target_os = "ios")))]
pub mod android;

#[cfg(any(target_os = "android", target_os = "ios"))]
pub(crate) use android as imp;
