//! Existing pane activation from JWM, without process creation.
use crate::AppModel;
use gtk::prelude::*;
use jterm_core::desktop_sessions::{Bridge, Session};
use relm4::{gtk, ComponentSender};
impl AppModel {
    pub(crate) fn poll_desktop_sessions(&mut self, sender: &ComponentSender<Self>) {
        if self.desktop_sessions.is_none() {
            let Some(surface) = self.window.surface() else {
                return;
            };
            let raw = if surface.type_().name().contains("X11") {
                let function = unsafe {
                    libc::dlsym(
                        libc::RTLD_DEFAULT,
                        b"gdk_x11_surface_get_xid\0".as_ptr().cast(),
                    )
                };
                if function.is_null() {
                    None
                } else {
                    let get: unsafe extern "C" fn(*mut libc::c_void) -> libc::c_ulong =
                        unsafe { std::mem::transmute(function) };
                    Some(unsafe { get(surface.as_ptr().cast()) } as u64)
                }
            } else {
                None
            };
            self.desktop_sessions = Some(Bridge::new("anvil", raw, || {}));
        }
        let identities = self
            .agent_session_tabs
            .iter()
            .filter_map(|(key, pane)| {
                if !self
                    .tabs
                    .iter()
                    .flat_map(|tab| &tab.panes)
                    .any(|p| p.id == *pane)
                {
                    return None;
                }
                let session = self.session_sidebar.desktop_session(key)?;
                let metadata = Session::from_agent(&session, pane.to_string())?;
                Some((metadata, key.clone(), session))
            })
            .collect::<Vec<_>>();
        self.desktop_sessions
            .as_ref()
            .unwrap()
            .publish(identities.iter().map(|(s, _, _)| s.clone()).collect());
        let requests = self.desktop_sessions.as_ref().unwrap().poll();
        for request in requests {
            let success = if let Some((_, key, session)) = identities.iter().find(|(s, _, _)| {
                s.session_id == request.session_id && s.pane_id == request.pane_id
            }) {
                let (completion, result) = std::sync::mpsc::channel();
                self.add_agent_session_tab(
                    crate::agent_sessions::ResumeRequest {
                        runtime_key: key.clone(),
                        provider: session.provider,
                        id: session.id.clone(),
                        title: session.title.clone(),
                        launch: None,
                        completion,
                    },
                    sender,
                );
                matches!(result.try_recv(), Ok(Ok(())))
            } else {
                false
            };
            self.desktop_sessions
                .as_ref()
                .unwrap()
                .acknowledge(request, success);
        }
    }
}
