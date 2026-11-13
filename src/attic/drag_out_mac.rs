//! macOS drag-out (#504): an AppKit dragging session of file promises.
//!
//! Each dragged row becomes an `NSFilePromiseProvider`. Finder decides where
//! the item lands and calls the provider's delegate to write it there; the
//! delegate hands the provider a background `NSOperationQueue`, so the
//! writing ([`PromiseDrag::write`]) never runs on the main thread. Finder
//! shows the files as "being copied" until each write completes.
//!
//! Lifetimes: a provider keeps its delegate in `userInfo` (the `delegate`
//! property is weak), the drag pasteboard keeps the providers, and the
//! dragging source stays in [`SOURCE`] until the next drag.

use super::{DragItem, PromiseDrag, SftpCommand, UnboundedSender};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::PathBuf;
use std::ptr::NonNull;
use std::sync::Arc;

use block2::DynBlock;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{
    define_class, msg_send, AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSEvent, NSEventModifierFlags, NSEventType, NSFilePromiseProvider,
    NSFilePromiseProviderDelegate, NSImage, NSView, NSWindow, NSWorkspace,
};
use objc2_foundation::{
    NSArray, NSError, NSOperationQueue, NSPoint, NSProcessInfo, NSRect, NSSize, NSString, NSURL,
};

thread_local! {
    /// The current drag's source; AppKit does not retain it for us.
    static SOURCE: RefCell<Option<Retained<DragSource>>> = const { RefCell::new(None) };
}

/// Start dragging `items` from `view` (winit's `NSView`). Returns once the
/// session has started.
pub(super) fn begin(
    view: NonNull<c_void>,
    commands: UnboundedSender<SftpCommand>,
    items: Vec<DragItem>,
) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        return false;
    };
    // SAFETY: winit's content view of a live window, used on the main thread.
    let view: &NSView = unsafe { view.cast().as_ref() };
    let Some(window) = view.window() else {
        return false;
    };
    let Some(event) = drag_event(mtm, &window) else {
        return false;
    };
    let Some(drag) = PromiseDrag::start(commands, &items) else {
        return false;
    };
    let drag = Arc::new(drag);
    let location = view.convertPoint_fromView(event.locationInWindow(), None);

    let mut dragging_items = Vec::with_capacity(items.len());
    for (i, item) in items.into_iter().enumerate() {
        let icon = icon_for(&item);
        let file_type = NSString::from_str(if item.is_dir {
            "public.folder"
        } else {
            "public.data"
        });
        let delegate = PromiseDelegate::new(drag.clone(), item);
        let provider = NSFilePromiseProvider::initWithFileType_delegate(
            NSFilePromiseProvider::alloc(),
            &file_type,
            ProtocolObject::from_ref(&*delegate),
        );
        let keep: &AnyObject = &delegate;
        // SAFETY: userInfo is an untyped strong reference AppKit never reads.
        unsafe { provider.setUserInfo(Some(keep)) };
        let dragging = NSDraggingItem::initWithPasteboardWriter(
            NSDraggingItem::alloc(),
            ProtocolObject::from_ref(&*provider),
        );
        // Fan several icons out a little under the pointer.
        let offset = 6.0 * i.min(5) as f64;
        let frame = NSRect::new(
            NSPoint::new(location.x - 16.0 + offset, location.y - 16.0 - offset),
            NSSize::new(32.0, 32.0),
        );
        let contents: &AnyObject = &icon;
        unsafe { dragging.setDraggingFrame_contents(frame, Some(contents)) };
        dragging_items.push(dragging);
    }

    let source = DragSource::new(mtm, drag);
    let dragging_items = NSArray::from_retained_slice(&dragging_items);
    let session = view.beginDraggingSessionWithItems_event_source(
        &dragging_items,
        &event,
        ProtocolObject::from_ref(&*source),
    );
    session.setAnimatesToStartingPositionsOnCancelOrFail(true);
    SOURCE.with(|slot| *slot.borrow_mut() = Some(source));
    true
}

/// The mouse event AppKit starts the session from: the one being handled,
/// or a synthesised drag at the pointer if there is none.
fn drag_event(mtm: MainThreadMarker, window: &NSWindow) -> Option<Retained<NSEvent>> {
    let current = NSApplication::sharedApplication(mtm).currentEvent();
    if let Some(event) = current {
        let kind = event.r#type();
        if kind == NSEventType::LeftMouseDragged || kind == NSEventType::LeftMouseDown {
            return Some(event);
        }
    }
    NSEvent::mouseEventWithType_location_modifierFlags_timestamp_windowNumber_context_eventNumber_clickCount_pressure(
        NSEventType::LeftMouseDragged,
        window.mouseLocationOutsideOfEventStream(),
        NSEventModifierFlags::empty(),
        NSProcessInfo::processInfo().systemUptime(),
        window.windowNumber(),
        None,
        0,
        1,
        1.0,
    )
}

/// Finder's icon for the item: a folder, or by file extension.
#[allow(deprecated)] // iconForContentType needs UniformTypeIdentifiers (macOS 11+).
fn icon_for(item: &DragItem) -> Retained<NSImage> {
    let kind = if item.is_dir {
        "public.folder".to_string()
    } else {
        std::path::Path::new(item.name())
            .extension()
            .and_then(|ext| ext.to_str())
            .map(str::to_string)
            .unwrap_or_else(|| "public.data".to_string())
    };
    NSWorkspace::sharedWorkspace().iconForFileType(&NSString::from_str(&kind))
}

// --- File promise delegate ------------------------------------------------

struct PromiseIvars {
    drag: Arc<PromiseDrag>,
    item: DragItem,
    queue: Retained<NSOperationQueue>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; the ivars are only
    // read, and PromiseDrag is Sync.
    #[unsafe(super(NSObject))]
    #[thread_kind = AllocAnyThread]
    #[name = "MeatShellFilePromiseDelegate"]
    #[ivars = PromiseIvars]
    struct PromiseDelegate;

    unsafe impl NSObjectProtocol for PromiseDelegate {}

    unsafe impl NSFilePromiseProviderDelegate for PromiseDelegate {
        #[unsafe(method_id(filePromiseProvider:fileNameForType:))]
        fn file_name_for_type(
            &self,
            _provider: &NSFilePromiseProvider,
            _file_type: &NSString,
        ) -> Retained<NSString> {
            NSString::from_str(self.ivars().item.name())
        }

        /// Runs on [`Self::operation_queue`]; blocking here is fine.
        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write_promise(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            completion: &DynBlock<dyn Fn(*mut NSError)>,
        ) {
            let ivars = self.ivars();
            let result = url
                .path()
                .map(|path| PathBuf::from(path.to_string()))
                .ok_or_else(|| "the drop target is not a folder".to_string())
                .and_then(|dest| ivars.drag.write(&ivars.item, &dest));
            match result {
                Ok(()) => completion.call((std::ptr::null_mut(),)),
                Err(_) => {
                    let domain = NSString::from_str("io.github.yituorou.meatshell");
                    let error = NSError::new(-1, &domain);
                    completion.call((Retained::as_ptr(&error) as *mut NSError,));
                }
            }
        }

        #[unsafe(method_id(operationQueueForFilePromiseProvider:))]
        fn operation_queue(&self, _provider: &NSFilePromiseProvider) -> Retained<NSOperationQueue> {
            self.ivars().queue.clone()
        }
    }
);

impl PromiseDelegate {
    fn new(drag: Arc<PromiseDrag>, item: DragItem) -> Retained<Self> {
        let queue = NSOperationQueue::new();
        let this = Self::alloc().set_ivars(PromiseIvars { drag, item, queue });
        unsafe { msg_send![super(this), init] }
    }
}

// --- Dragging source --------------------------------------------------------

struct SourceIvars {
    drag: Arc<PromiseDrag>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MeatShellDragSource"]
    #[ivars = SourceIvars]
    struct DragSource;

    unsafe impl NSObjectProtocol for DragSource {}

    unsafe impl NSDraggingSource for DragSource {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }

        #[unsafe(method(draggingSession:endedAtPoint:operation:))]
        fn ended(&self, _session: &NSDraggingSession, _point: NSPoint, operation: NSDragOperation) {
            // Nothing was dropped, so no promise will be written: let go of
            // the SFTP worker now. After a drop, the last write does it.
            if operation == NSDragOperation::None {
                self.ivars().drag.finish();
            }
        }
    }
);

impl DragSource {
    fn new(mtm: MainThreadMarker, drag: Arc<PromiseDrag>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(SourceIvars { drag });
        unsafe { msg_send![super(this), init] }
    }
}
