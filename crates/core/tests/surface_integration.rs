//! Integration tests for the surface abstraction a non-terminal host builds on:
//! descriptor validation/serialization and registry selection.

use std::sync::Arc;

use coderex_core::surface::{
    SurfaceCapabilities, SurfaceDescriptor, SurfaceError, SurfaceId, SurfaceRegistry, UiSurface,
};
use coderex_core::ui_protocol::{UiElement, UiStyle};

#[derive(Debug)]
struct TextSurface(SurfaceDescriptor);

impl UiSurface for TextSurface {
    fn descriptor(&self) -> &SurfaceDescriptor {
        &self.0
    }

    fn render_text(&self, elements: &[UiElement], width: usize) -> String {
        format!("{}@{width}:{}", self.0.id, elements.len())
    }
}

fn surface(id: &str, capabilities: SurfaceCapabilities) -> Arc<dyn UiSurface> {
    Arc::new(TextSurface(
        SurfaceDescriptor::new(id, capabilities).expect("valid id"),
    ))
}

#[test]
fn a_descriptor_round_trips_through_json() {
    let descriptor = SurfaceDescriptor::new(
        "web",
        SurfaceCapabilities {
            interactive: true,
            mouse: false,
            images: true,
        },
    )
    .expect("valid");

    let json = serde_json::to_string(&descriptor).expect("serialize");
    assert_eq!(
        json,
        r#"{"id":"web","capabilities":{"interactive":true,"mouse":false,"images":true}}"#
    );
    let decoded: SurfaceDescriptor = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(decoded, descriptor);

    let terminal: SurfaceDescriptor = serde_json::from_str(
        r#"{"id":"terminal","capabilities":{"interactive":true,"mouse":true,"images":false}}"#,
    )
    .expect("the built-in descriptor parses");
    assert_eq!(terminal, SurfaceDescriptor::terminal());
}

#[test]
fn surface_ids_are_validated() {
    assert_eq!(SurfaceId::new("terminal").unwrap().as_str(), "terminal");
    assert_eq!(
        SurfaceId::new("ide_vs-code-2").unwrap().as_str(),
        "ide_vs-code-2"
    );

    assert!(matches!(SurfaceId::new(""), Err(SurfaceError::Empty)));
    assert!(matches!(
        SurfaceId::new("Web UI"),
        Err(SurfaceError::InvalidCharacter)
    ));
    assert!(matches!(
        SurfaceId::new("x".repeat(65)),
        Err(SurfaceError::TooLong)
    ));
}

#[test]
fn the_first_registered_surface_is_active_and_selection_moves() {
    let mut registry = SurfaceRegistry::new();
    assert!(registry.active_descriptor().is_none());

    registry
        .register(surface("terminal", SurfaceCapabilities::TERMINAL))
        .expect("register");
    registry
        .register(surface("web", SurfaceCapabilities::TEXT_ONLY))
        .expect("register");
    assert_eq!(
        registry.active_descriptor().map(|d| d.id),
        Some(SurfaceId::terminal())
    );
    assert_eq!(registry.descriptors().len(), 2);

    registry
        .set_active(&SurfaceId::new("web").unwrap())
        .expect("known");
    let active = registry.active().expect("active");
    assert_eq!(active.descriptor().id.as_str(), "web");
    let rendered = active.render_text(
        &[UiElement::Text {
            text: "hi".into(),
            style: UiStyle::Plain,
        }],
        40,
    );
    assert_eq!(rendered, "web@40:1");
}

#[test]
fn duplicate_and_unknown_ids_are_rejected() {
    let mut registry = SurfaceRegistry::new();
    registry
        .register(surface("terminal", SurfaceCapabilities::TERMINAL))
        .expect("register");

    let duplicate = registry
        .register(surface("terminal", SurfaceCapabilities::TEXT_ONLY))
        .expect_err("duplicate id");
    assert!(matches!(duplicate, SurfaceError::Duplicate(id) if id == "terminal"));

    let unknown = registry
        .set_active(&SurfaceId::new("absent").unwrap())
        .expect_err("unknown id");
    assert!(matches!(unknown, SurfaceError::Unknown(id) if id == "absent"));

    assert_eq!(
        registry.active_descriptor().map(|d| d.id),
        Some(SurfaceId::terminal()),
        "a failed selection leaves the previous surface active"
    );
}
