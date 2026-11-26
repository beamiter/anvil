use gtk::prelude::*;
use relm4::gtk;
#[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
use vte4::TerminalExt;

/// Identity of a selectable surface in the ordered card document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TerminalTextSurface {
    #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
    Native(vte4::Terminal),
    #[cfg(anvil_shared_terminal)]
    Document(gtk::TextView),
}

impl TerminalTextSurface {
    pub fn widget(&self) -> &gtk::Widget {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(terminal) => terminal.upcast_ref(),
            #[cfg(anvil_shared_terminal)]
            Self::Document(text) => text.upcast_ref(),
        }
    }

    pub fn from_widget(widget: gtk::Widget) -> Option<Self> {
        #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
        let widget = match widget.downcast::<vte4::Terminal>() {
            Ok(terminal) => return Some(Self::Native(terminal)),
            Err(widget) => widget,
        };
        #[cfg(anvil_shared_terminal)]
        {
            widget.downcast::<gtk::TextView>().ok().map(Self::Document)
        }
        #[cfg(not(anvil_shared_terminal))]
        {
            None
        }
    }

    pub fn is_native(&self) -> bool {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(_) => true,
            #[cfg(anvil_shared_terminal)]
            Self::Document(_) => false,
        }
    }

    #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
    pub fn native(&self) -> Option<&vte4::Terminal> {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(terminal) => Some(terminal),
            #[cfg(anvil_shared_terminal)]
            Self::Document(_) => None,
        }
    }

    #[cfg(anvil_shared_terminal)]
    pub fn document(&self) -> Option<&gtk::TextView> {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(_) => None,
            Self::Document(view) => Some(view),
        }
    }

    pub fn has_selection(&self) -> bool {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(terminal) => terminal.has_selection(),
            #[cfg(anvil_shared_terminal)]
            Self::Document(text) => text.buffer().has_selection(),
        }
    }

    pub fn clear_selection(&self) {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(terminal) => terminal.unselect_all(),
            #[cfg(anvil_shared_terminal)]
            Self::Document(text) => {
                let buffer = text.buffer();
                buffer.place_cursor(&buffer.start_iter());
            }
        }
    }

    pub fn document_text(&self) -> Option<String> {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(_) => None,
            #[cfg(anvil_shared_terminal)]
            Self::Document(view) => {
                let buffer = view.buffer();
                Some(
                    buffer
                        .text(&buffer.start_iter(), &buffer.end_iter(), true)
                        .to_string(),
                )
            }
        }
    }

    pub fn selected_text(&self) -> Option<String> {
        match self {
            #[cfg(any(anvil_vte_oracle, anvil_legacy_renderer))]
            Self::Native(terminal) => crate::vte_reference::selected_text(terminal),
            #[cfg(anvil_shared_terminal)]
            Self::Document(text) => {
                let buffer = text.buffer();
                let (start, end) = buffer.selection_bounds()?;
                Some(buffer.text(&start, &end, true).to_string())
            }
        }
    }
}
