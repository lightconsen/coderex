//! The terminal surface: the only surface CodeRex ships.
//!
//! The abstraction lives in [`coderex_core::surface`]; this module is its
//! terminal implementation. It reuses the plain-text renderer so headless
//! transcripts, wire clients and tests present mod UI exactly as the TUI's
//! compact form does.

use coderex_core::surface::{SurfaceDescriptor, UiSurface};
use coderex_core::ui_protocol::UiElement;

use crate::mod_elements::render_mod_ui_text;

/// The terminal surface.
///
/// Keyboard focus and mouse clicks can activate buttons; there is no image
/// protocol, so `images` is false.
#[derive(Debug, Clone)]
pub struct TerminalSurface {
    descriptor: SurfaceDescriptor,
}

impl TerminalSurface {
    /// Build the terminal surface descriptor.
    #[must_use]
    pub fn new() -> Self {
        Self {
            descriptor: SurfaceDescriptor::terminal(),
        }
    }
}

impl Default for TerminalSurface {
    fn default() -> Self {
        Self::new()
    }
}

impl UiSurface for TerminalSurface {
    fn descriptor(&self) -> &SurfaceDescriptor {
        &self.descriptor
    }

    fn render_text(&self, elements: &[UiElement], width: usize) -> String {
        render_mod_ui_text(elements, width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use coderex_core::surface::UiSurface;
    use coderex_core::ui_protocol::UiStyle;

    #[test]
    fn the_terminal_surface_names_itself_and_reports_its_capabilities() {
        let surface = TerminalSurface::new();
        let descriptor = surface.descriptor();
        assert_eq!(descriptor.id.as_str(), "terminal");
        assert!(descriptor.capabilities.interactive);
        assert!(descriptor.capabilities.mouse);
        assert!(!descriptor.capabilities.images);
    }

    #[test]
    fn the_terminal_surface_renders_the_shared_text_form() {
        let surface = TerminalSurface::default();
        let elements = vec![
            UiElement::Text {
                text: "hello".to_string(),
                style: UiStyle::Plain,
            },
            UiElement::Button {
                label: "Retry".to_string(),
                id: "retry".to_string(),
            },
        ];
        let rendered = surface.render_text(&elements, 40);
        assert!(rendered.contains("hello"), "{rendered}");
        assert!(rendered.contains("Retry"), "{rendered}");
    }
}
