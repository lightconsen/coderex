//! Front-end surfaces: the host side of the session contract.
//!
//! A **surface** is the product a session is presented in: the terminal, an
//! IDE panel, a desktop window, a web page, a chat product. The engine and the
//! mod runtime know nothing about rendering. They emit `EngineEvent`s (see
//! [`crate::types::EngineEvent`]) and, for mods, host-owned
//! [`UiElement`] trees. A surface names itself
//! and knows how to present those trees.
//!
//! The abstraction is deliberately small:
//!
//! - [`SurfaceId`] is the stable name a mod sees as `e.surface`.
//! - [`SurfaceCapabilities`] says what the surface can do, so a mod can branch
//!   before rendering an affordance the surface cannot show.
//! - [`UiSurface`] is the trait a host implements.
//! - [`SurfaceRegistry`] holds the surfaces a host can present and which one is
//!   active.
//!
//! CodeRex ships one implementation, the terminal
//! ([`coderex_ui::surface::TerminalSurface`](../coderex_ui/surface/struct.TerminalSurface.html)).
//! Adding another is a host-side project: implement [`UiSurface`], register it,
//! and render the same element tree for that medium. No engine or mod change is
//! required, and the id travels to mods through
//! [`ModSession`](crate::surface::SurfaceDescriptor) instead of being a
//! constant.

use std::fmt;
use std::str::FromStr;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ui_protocol::UiElement;

/// Longest surface id the registry accepts.
const MAX_SURFACE_ID_LEN: usize = 32;

/// Why a surface id or registration was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SurfaceError {
    /// The id was empty.
    #[error("a surface id must not be empty")]
    Empty,
    /// The id was longer than `MAX_SURFACE_ID_LEN`.
    #[error("a surface id may be at most {MAX_SURFACE_ID_LEN} characters")]
    TooLong,
    /// The id contains a character outside `[a-z0-9_-]`.
    #[error("a surface id may only contain lowercase ASCII letters, digits, '-' and '_'")]
    InvalidCharacter,
    /// A surface with this id is already registered.
    #[error("surface '{0}' is already registered")]
    Duplicate(String),
    /// No surface with this id is registered.
    #[error("no surface named '{0}' is registered")]
    Unknown(String),
}

/// The stable name of a front-end surface.
///
/// This is the value a mod sees as `e.surface`. Ids are lowercase so they are
/// comparable across hosts; [`SurfaceId::TERMINAL`] is the one CodeRex ships.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SurfaceId(String);

impl SurfaceId {
    /// The terminal surface's id.
    pub const TERMINAL: &'static str = "terminal";

    /// Validate and wrap a surface id.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] when the id is empty, too long, or contains a
    /// character outside `[a-z0-9_-]`.
    pub fn new(value: impl Into<String>) -> Result<Self, SurfaceError> {
        let value = value.into();
        if value.is_empty() {
            return Err(SurfaceError::Empty);
        }
        if value.chars().count() > MAX_SURFACE_ID_LEN {
            return Err(SurfaceError::TooLong);
        }
        if !value
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            return Err(SurfaceError::InvalidCharacter);
        }
        Ok(Self(value))
    }

    /// The terminal surface id. Cannot fail.
    #[must_use]
    pub fn terminal() -> Self {
        Self(Self::TERMINAL.to_string())
    }

    /// The id as a string slice.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SurfaceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for SurfaceId {
    type Err = SurfaceError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

/// What a surface can present.
///
/// Mods receive this as `e.surfaceCapabilities`; a host uses it to decide which
/// affordances to advertise. It is data, not policy: a surface that reports
/// `interactive: false` still accepts activation through another surface if
/// the host routes it there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceCapabilities {
    /// Buttons can be focused and activated.
    pub interactive: bool,
    /// Pointer events can reach buttons.
    pub mouse: bool,
    /// The surface can render images.
    pub images: bool,
}

impl SurfaceCapabilities {
    /// The terminal: keyboard focus and mouse clicks, no image protocol.
    pub const TERMINAL: Self = Self {
        interactive: true,
        mouse: true,
        images: false,
    };

    /// A surface that can only print text.
    pub const TEXT_ONLY: Self = Self {
        interactive: false,
        mouse: false,
        images: false,
    };
}

/// The identity of the surface a session is presented in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SurfaceDescriptor {
    /// The stable id a mod sees as `e.surface`.
    pub id: SurfaceId,
    /// What this surface can present.
    pub capabilities: SurfaceCapabilities,
}

impl SurfaceDescriptor {
    /// The terminal descriptor CodeRex ships.
    #[must_use]
    pub fn terminal() -> Self {
        Self {
            id: SurfaceId::terminal(),
            capabilities: SurfaceCapabilities::TERMINAL,
        }
    }

    /// A descriptor for a host-defined surface.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError`] when the id is invalid.
    pub fn new(
        id: impl Into<String>,
        capabilities: SurfaceCapabilities,
    ) -> Result<Self, SurfaceError> {
        Ok(Self {
            id: SurfaceId::new(id)?,
            capabilities,
        })
    }
}

impl Default for SurfaceDescriptor {
    fn default() -> Self {
        Self::terminal()
    }
}

/// A front-end surface a host can present a session in.
///
/// Implement this for a new product surface. The engine hands the surface
/// [`UiElement`] trees through [`EngineEvent::ModUi`](crate::types::EngineEvent)
/// — rich surfaces render them natively; [`Self::render_text`] is the
/// lowest-common-denominator presentation used by headless transcripts, wire
/// clients and tests.
pub trait UiSurface: fmt::Debug + Send + Sync {
    /// The surface's id and capabilities.
    fn descriptor(&self) -> &SurfaceDescriptor;

    /// Render one mod UI block as text.
    ///
    /// `width` is the surface's usable text width in columns. A rich surface
    /// may ignore this and use its own widgets, but it must keep the result
    /// usable as a fallback (logs, transcripts, wire clients).
    fn render_text(&self, elements: &[UiElement], width: usize) -> String;
}

/// The surfaces a host can present, and which one is active.
///
/// A session has exactly one active surface. Registering another surface does
/// not change the active one; [`Self::set_active`] does.
#[derive(Debug, Default)]
pub struct SurfaceRegistry {
    surfaces: Vec<Arc<dyn UiSurface>>,
    active: Option<SurfaceId>,
}

impl SurfaceRegistry {
    /// An empty registry.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            surfaces: Vec::new(),
            active: None,
        }
    }

    /// Add a surface. The first registered surface becomes active.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError::Duplicate`] when the id is already registered.
    pub fn register(&mut self, surface: Arc<dyn UiSurface>) -> Result<(), SurfaceError> {
        let id = surface.descriptor().id.clone();
        if self.surfaces.iter().any(|held| held.descriptor().id == id) {
            return Err(SurfaceError::Duplicate(id.to_string()));
        }
        if self.active.is_none() {
            self.active = Some(id);
        }
        self.surfaces.push(surface);
        Ok(())
    }

    /// Make a registered surface the active one.
    ///
    /// # Errors
    ///
    /// Returns [`SurfaceError::Unknown`] when the id is not registered.
    pub fn set_active(&mut self, id: &SurfaceId) -> Result<(), SurfaceError> {
        if !self.surfaces.iter().any(|held| held.descriptor().id == *id) {
            return Err(SurfaceError::Unknown(id.to_string()));
        }
        self.active = Some(id.clone());
        Ok(())
    }

    /// The active surface, if one is registered.
    #[must_use]
    pub fn active(&self) -> Option<&Arc<dyn UiSurface>> {
        let id = self.active.as_ref()?;
        self.get(id)
    }

    /// A registered surface by id.
    #[must_use]
    pub fn get(&self, id: &SurfaceId) -> Option<&Arc<dyn UiSurface>> {
        self.surfaces
            .iter()
            .find(|held| held.descriptor().id == *id)
    }

    /// Every registered descriptor, in registration order.
    #[must_use]
    pub fn descriptors(&self) -> Vec<SurfaceDescriptor> {
        self.surfaces
            .iter()
            .map(|held| held.descriptor().clone())
            .collect()
    }

    /// The active surface's descriptor.
    #[must_use]
    pub fn active_descriptor(&self) -> Option<SurfaceDescriptor> {
        self.active().map(|surface| surface.descriptor().clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct TestSurface(SurfaceDescriptor);

    impl UiSurface for TestSurface {
        fn descriptor(&self) -> &SurfaceDescriptor {
            &self.0
        }

        fn render_text(&self, elements: &[UiElement], _width: usize) -> String {
            format!("test:{}", elements.len())
        }
    }

    fn test_surface(id: &str) -> Arc<dyn UiSurface> {
        Arc::new(TestSurface(
            SurfaceDescriptor::new(id, SurfaceCapabilities::TEXT_ONLY).unwrap(),
        ))
    }

    #[test]
    fn surface_ids_are_validated() {
        assert_eq!(SurfaceId::new("").unwrap_err(), SurfaceError::Empty);
        assert_eq!(
            SurfaceId::new("x".repeat(MAX_SURFACE_ID_LEN + 1)).unwrap_err(),
            SurfaceError::TooLong
        );
        assert_eq!(
            SurfaceId::new("Terminal").unwrap_err(),
            SurfaceError::InvalidCharacter
        );
        assert_eq!(
            SurfaceId::new("ide-panel_2").unwrap().as_str(),
            "ide-panel_2"
        );
        assert_eq!(SurfaceId::terminal().as_str(), SurfaceId::TERMINAL);
    }

    #[test]
    fn the_terminal_descriptor_is_interactive_but_has_no_images() {
        let descriptor = SurfaceDescriptor::terminal();
        assert_eq!(descriptor.id.as_str(), "terminal");
        assert!(descriptor.capabilities.interactive);
        assert!(descriptor.capabilities.mouse);
        assert!(!descriptor.capabilities.images);
    }

    #[test]
    fn the_first_registered_surface_becomes_active() {
        let mut registry = SurfaceRegistry::new();
        assert!(registry.active().is_none());
        registry.register(test_surface("terminal")).unwrap();
        registry.register(test_surface("ide")).unwrap();
        assert_eq!(
            registry.active().unwrap().descriptor().id.as_str(),
            "terminal"
        );
    }

    #[test]
    fn registering_the_same_id_twice_is_refused() {
        let mut registry = SurfaceRegistry::new();
        registry.register(test_surface("terminal")).unwrap();
        assert_eq!(
            registry.register(test_surface("terminal")).unwrap_err(),
            SurfaceError::Duplicate("terminal".to_string())
        );
    }

    #[test]
    fn activating_an_unknown_surface_is_refused() {
        let mut registry = SurfaceRegistry::new();
        registry.register(test_surface("terminal")).unwrap();
        let unknown = SurfaceId::new("ide").unwrap();
        assert_eq!(
            registry.set_active(&unknown).unwrap_err(),
            SurfaceError::Unknown("ide".to_string())
        );
    }

    #[test]
    fn a_surface_can_be_selected_and_described() {
        let mut registry = SurfaceRegistry::new();
        registry.register(test_surface("terminal")).unwrap();
        registry.register(test_surface("ide")).unwrap();
        let ide = SurfaceId::new("ide").unwrap();
        registry.set_active(&ide).unwrap();
        assert_eq!(registry.active_descriptor().unwrap().id.as_str(), "ide");
        assert_eq!(registry.descriptors().len(), 2);
    }
}
