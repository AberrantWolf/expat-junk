//! Receive Finder open-document events without replacing winit's app delegate.

use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::NSApplicationWillFinishLaunchingNotification;
use objc2_foundation::{
    MainThreadMarker, NSAppleEventDescriptor, NSAppleEventManager, NSNotification,
    NSNotificationCenter, NSObject, NSObjectProtocol,
};
use std::path::PathBuf;

use super::OpenRequests;

const CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
const OPEN_DOCUMENTS: u32 = u32::from_be_bytes(*b"odoc");
const DIRECT_OBJECT: u32 = u32::from_be_bytes(*b"----");
const FILE_URL: u32 = u32::from_be_bytes(*b"furl");
const ALIAS: u32 = u32::from_be_bytes(*b"alis");
const BOOKMARK: u32 = u32::from_be_bytes(*b"bmrk");
const FILE_REF: u32 = u32::from_be_bytes(*b"fsrf");

define_class!(
    // SAFETY: NSObject permits subclassing; methods and access stay on the main thread.
    #[unsafe(super = NSObject)]
    #[name = "ExpatJunkOpenDocumentsHandler"]
    #[thread_kind = MainThreadOnly]
    #[ivars = OpenRequests]
    struct Handler;

    unsafe impl NSObjectProtocol for Handler {}

    impl Handler {
        #[unsafe(method(applicationWillFinishLaunching:))]
        fn will_finish_launching(&self, _notification: &NSNotification) {
            // AppKit installs its own Apple Event handlers during startup. Override
            // only open-documents after that setup and before launch events arrive.
            self.register();
        }

        #[unsafe(method(openDocuments:withReplyEvent:))]
        fn open_documents(&self, event: &NSAppleEventDescriptor, _reply: Option<&NSAppleEventDescriptor>) {
            self.ivars().request(requested_paths(event));
        }
    }
);

fn requested_paths(event: &NSAppleEventDescriptor) -> Vec<PathBuf> {
    let Some(files) = event.paramDescriptorForKeyword(DIRECT_OBJECT) else {
        return Vec::new();
    };
    (1..=files.numberOfItems())
        .filter_map(|index| {
            let descriptor = files.descriptorAtIndex(index)?;
            // fileURLValue also coerces arbitrary strings as legacy HFS paths.
            // Only accept actual file descriptors supplied by the open event.
            if !matches!(
                descriptor.descriptorType(),
                FILE_URL | ALIAS | FILE_REF | BOOKMARK
            ) {
                return None;
            }
            descriptor.fileURLValue()?.to_file_path()
        })
        .collect()
}

impl Handler {
    fn register(&self) {
        // SAFETY: The selector above accepts the two descriptors supplied by
        // NSAppleEventManager, and OpenFileEvents retains this receiver.
        unsafe {
            NSAppleEventManager::sharedAppleEventManager()
                .setEventHandler_andSelector_forEventClass_andEventID(
                    self,
                    sel!(openDocuments:withReplyEvent:),
                    CORE_EVENT_CLASS,
                    OPEN_DOCUMENTS,
                );
        }
    }
}

/// Keep alive around run_native; unregister before the retained handler is dropped.
pub struct OpenFileEvents(Retained<Handler>);

impl OpenFileEvents {
    pub fn install(requests: OpenRequests) -> Self {
        let mtm = MainThreadMarker::new().expect("desktop startup runs on the main thread");
        // SAFETY: NSObject's init signature is correct and initializes the allocated receiver.
        let handler: Retained<Handler> =
            unsafe { msg_send![super(Handler::alloc(mtm).set_ivars(requests)), init] };
        // SAFETY: Launch notifications are delivered on the main thread, this
        // selector has the expected signature, and Self keeps the observer alive.
        unsafe {
            NSNotificationCenter::defaultCenter().addObserver_selector_name_object(
                &handler,
                sel!(applicationWillFinishLaunching:),
                Some(NSApplicationWillFinishLaunchingNotification),
                None,
            );
        }
        Self(handler)
    }
}

impl Drop for OpenFileEvents {
    fn drop(&mut self) {
        // SAFETY: The registered observer is this same retained object.
        unsafe { NSNotificationCenter::defaultCenter().removeObserver(&self.0) };
        NSAppleEventManager::sharedAppleEventManager()
            .removeEventHandlerForEventClass_andEventID(CORE_EVENT_CLASS, OPEN_DOCUMENTS);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_foundation::{NSURL, NSURLBookmarkCreationOptions, ns_string};

    #[test]
    fn apple_event_decodes_file_urls_with_spaces_unicode_and_percent_signs() {
        let event = NSAppleEventDescriptor::appleEventWithEventClass_eventID_targetDescriptor_returnID_transactionID(
            CORE_EVENT_CLASS, OPEN_DOCUMENTS, None, -1, 0,
        );
        assert!(requested_paths(&event).is_empty());
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let paths = [root.join("書類 50% #1.pdf"), root.join("work.EXPJ")];
        let list = NSAppleEventDescriptor::listDescriptor();
        for (index, path) in paths.iter().enumerate() {
            std::fs::write(path, b"fixture").unwrap();
            let url = NSURL::from_file_path(path).unwrap();
            let descriptor = NSAppleEventDescriptor::descriptorWithFileURL(&url);
            // Modern Launch Services sends bookmark descriptors, not file URLs.
            let descriptor = if index == 1 {
                let data = url
                    .bookmarkDataWithOptions_includingResourceValuesForKeys_relativeToURL_error(
                        NSURLBookmarkCreationOptions::MinimalBookmark,
                        None,
                        None,
                    )
                    .unwrap();
                NSAppleEventDescriptor::descriptorWithDescriptorType_data(BOOKMARK, Some(&data))
                    .unwrap()
            } else {
                descriptor
            };
            list.insertDescriptor_atIndex(&descriptor, (index + 1) as isize);
        }
        // Non-file descriptors must not turn into local filesystem paths.
        list.insertDescriptor_atIndex(
            &NSAppleEventDescriptor::descriptorWithString(ns_string!(
                "https://example.com/document.pdf"
            )),
            3,
        );
        event.setParamDescriptor_forKeyword(&list, DIRECT_OBJECT);
        assert_eq!(requested_paths(&event), paths);
    }
}
