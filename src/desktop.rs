//! Desktop open requests, queued independently of the GUI and document workers.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[cfg(target_os = "macos")]
pub mod macos;

type Waker = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
struct Inbox {
    paths: Vec<PathBuf>,
    waker: Option<Waker>,
}

/// Keeps launch requests until the app is ready, and wakes an idle app for later
/// OS events. The caller decides when it is safe to consume a request.
#[derive(Clone, Default)]
pub struct OpenRequests(Arc<Mutex<Inbox>>);

impl OpenRequests {
    pub fn request(&self, paths: impl IntoIterator<Item = PathBuf>) {
        let waker = {
            let mut inbox = self.0.lock().unwrap_or_else(|e| e.into_inner());
            for path in paths {
                if !inbox.paths.contains(&path) {
                    inbox.paths.push(path);
                }
            }
            inbox.waker.clone()
        };
        if let Some(waker) = waker {
            waker();
        }
    }

    pub fn startup_arguments(&self, arguments: impl IntoIterator<Item = OsString>) {
        let mut literal = false;
        let mut paths = Vec::new();
        for argument in arguments {
            if !literal && argument == "--" {
                literal = true;
                continue;
            }
            // Older Finder launches can include a process serial number.
            #[cfg(target_os = "macos")]
            if !literal && argument.as_encoded_bytes().starts_with(b"-psn_") {
                continue;
            }
            paths.push(PathBuf::from(argument));
        }
        self.request(paths);
    }

    pub fn set_waker(&self, waker: Waker) {
        let pending = {
            let mut inbox = self.0.lock().unwrap_or_else(|e| e.into_inner());
            inbox.waker = Some(waker.clone());
            !inbox.paths.is_empty()
        };
        if pending {
            waker();
        }
    }

    pub fn take(&self) -> Result<Option<PathBuf>, &'static str> {
        let mut inbox = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let mut paths = std::mem::take(&mut inbox.paths);
        match paths.len() {
            0 => Ok(None),
            1 => Ok(paths.pop()),
            _ => Err("Open one document at a time; multiple files were requested."),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn launch_paths_survive_until_the_ui_attaches_and_os_requests_wake_it() {
        let requests = OpenRequests::default();
        let path = PathBuf::from("書類 with spaces.pdf");
        requests.startup_arguments([OsString::from("--"), path.clone().into_os_string()]);
        // Finder and argv may supply the same launch path.
        requests.request([path.clone()]);
        let wakes = Arc::new(AtomicUsize::new(0));
        let count = wakes.clone();
        requests.set_waker(Arc::new(move || {
            count.fetch_add(1, Ordering::Relaxed);
        }));
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        assert_eq!(requests.take().unwrap(), Some(path));
        assert_eq!(requests.take().unwrap(), None);
        requests.request([PathBuf::from("next.expj")]);
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
        assert_eq!(requests.take().unwrap(), Some(PathBuf::from("next.expj")));
    }

    #[test]
    fn multiple_files_are_reported_instead_of_silently_replacing_each_other() {
        let requests = OpenRequests::default();
        requests.startup_arguments(["one.pdf".into(), "two.pdf".into()]);
        assert!(requests.take().is_err());
        assert_eq!(requests.take().unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_not_lossily_decoded() {
        use std::os::unix::ffi::OsStringExt;
        let path = OsString::from_vec(b"document-\xff.pdf".to_vec());
        let requests = OpenRequests::default();
        requests.startup_arguments([path.clone()]);
        assert_eq!(requests.take().unwrap(), Some(PathBuf::from(path)));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn finder_process_argument_is_not_treated_as_a_document() {
        let requests = OpenRequests::default();
        requests.startup_arguments(["-psn_0_12345".into(), "document.pdf".into()]);
        assert_eq!(
            requests.take().unwrap(),
            Some(PathBuf::from("document.pdf"))
        );
    }
}
