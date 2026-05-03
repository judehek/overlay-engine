//! Game -> WebView2 input forwarding.
//!
//! * [`hit_test`] — engine-side, runs against the current `&[HitRegion]`
//!   to decide whether a cursor event should be forwarded to the
//!   WebView2 thread. Replaces `web.rs`'s single hard-coded overlay rect
//!   with the multi-rect [`HitRegion`](crate::hit_region::HitRegion)
//!   model the public API exposes.
//! * [`dispatch`] — WebView2-thread-side, translates an [`InputEvent`]
//!   into the appropriate `ICoreWebView2CompositionController` call.
//!
//! [`InputEvent`]: asdf_overlay_common::event::input::InputEvent

pub(crate) mod dispatch;
pub(crate) mod hit_test;

pub(crate) use dispatch::dispatch_input;
