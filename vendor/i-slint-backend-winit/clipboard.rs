// Copyright © SixtyFPS GmbH <info@slint.dev>
// SPDX-License-Identifier: GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR LicenseRef-Slint-Software-3.0

use copypasta::ClipboardProvider;

/// The Default, and the selection clippoard
pub type ClipboardPair = (Box<dyn ClipboardProvider>, Box<dyn ClipboardProvider>);

pub fn select_clipboard(
    pair: &mut ClipboardPair,
    clipboard: i_slint_core::platform::Clipboard,
) -> Option<&mut dyn ClipboardProvider> {
    match clipboard {
        i_slint_core::platform::Clipboard::DefaultClipboard => Some(pair.0.as_mut()),
        i_slint_core::platform::Clipboard::SelectionClipboard => Some(pair.1.as_mut()),
        _ => None,
    }
}

pub fn create_clipboard(
    _display_handle: &winit::raw_window_handle::DisplayHandle<'_>,
) -> ClipboardPair {
    // Provide a truly silent no-op clipboard context, as copypasta's NoopClipboard spams stdout with
    // println.
    struct SilentClipboardContext;
    impl ClipboardProvider for SilentClipboardContext {
        fn get_contents(
            &mut self,
        ) -> Result<String, Box<dyn std::error::Error + Send + Sync + 'static>> {
            Ok(Default::default())
        }

        fn set_contents(
            &mut self,
            _: String,
        ) -> Result<(), Box<dyn std::error::Error + Send + Sync + 'static>> {
            Ok(())
        }
    }

    cfg_if::cfg_if! {
        if #[cfg(all(
            unix,
            not(any(
                target_vendor = "apple",
                target_os = "android",
                target_os = "emscripten"
            ))
        ))]
        {

            #[cfg(feature = "wayland")]
            if let raw_window_handle::RawDisplayHandle::Wayland(wayland) = _display_handle.as_raw() {
                let clipboard = unsafe { copypasta::wayland_clipboard::create_clipboards_from_external(wayland.display.as_ptr()) };
                return (Box::new(clipboard.1), Box::new(clipboard.0));
            };
            #[cfg(feature = "x11")]
            {
                use copypasta::x11_clipboard::{X11ClipboardContext, Primary, Clipboard};
                let prim = X11ClipboardContext::<Primary>::new()
                    .map_or(
                        Box::new(SilentClipboardContext) as Box<dyn ClipboardProvider>,
                        |x| Box::new(x) as Box<dyn ClipboardProvider>,
                    );
                let sec = X11ClipboardContext::<Clipboard>::new()
                    .map_or(
                        Box::new(SilentClipboardContext) as Box<dyn ClipboardProvider>,
                        |x| Box::new(x) as Box<dyn ClipboardProvider>,
                    );
                (sec, prim)
            }
            #[cfg(not(feature = "x11"))]
            (Box::new(SilentClipboardContext), Box::new(SilentClipboardContext))
        } else {
            type ClipboardError = Box<dyn std::error::Error + Send + Sync + 'static>;

            /// On Windows the clipboard is a shared resource that another process
            /// (the IME, clipboard history, a sync tool, our own terminal-copy
            /// thread) can hold open for a short moment, and the raw Win32 open
            /// fails instantly in that window. The plain backend tried exactly once
            /// and the caller swallowed the error, so a copy silently did nothing
            /// until the next lucky attempt. Retry a bounded number of times (at
            /// most 7 * 10ms) and log the final failure instead.
            struct RetryingClipboard<T: ClipboardProvider> {
                inner: T,
            }

            impl<T: ClipboardProvider> RetryingClipboard<T> {
                const ATTEMPTS: usize = 8;

                fn retry<R>(
                    &mut self,
                    name: &str,
                    mut op: impl FnMut(&mut T) -> Result<R, ClipboardError>,
                ) -> Result<R, ClipboardError> {
                    let mut last_err = None;
                    for attempt in 0..Self::ATTEMPTS {
                        match op(&mut self.inner) {
                            Ok(value) => return Ok(value),
                            Err(err) => last_err = Some(err),
                        }
                        if attempt + 1 < Self::ATTEMPTS {
                            std::thread::sleep(std::time::Duration::from_millis(10));
                        }
                    }
                    let err = last_err.expect("at least one attempt ran");
                    tracing::warn!(
                        "clipboard {name} failed after {} attempts: {err}",
                        Self::ATTEMPTS
                    );
                    Err(err)
                }
            }

            impl<T: ClipboardProvider> ClipboardProvider for RetryingClipboard<T> {
                fn get_contents(&mut self) -> Result<String, ClipboardError> {
                    self.retry("get", |inner| inner.get_contents())
                }

                fn set_contents(&mut self, data: String) -> Result<(), ClipboardError> {
                    self.retry("set", move |inner| inner.set_contents(data.clone()))
                }
            }

            (
                copypasta::ClipboardContext::new().map_or(
                    Box::new(SilentClipboardContext) as Box<dyn ClipboardProvider>,
                    |x| {
                        Box::new(RetryingClipboard { inner: x }) as Box<dyn ClipboardProvider>
                    },
                ),
                Box::new(SilentClipboardContext),
            )
        }
    }
}
