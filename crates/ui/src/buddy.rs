//! Buddy Companion — deterministic AI companion generation from user ID.
//!
//! A buddy has a persistent soul (personality, name, colors) derived from the user's ID,
//! and session-local bones (mood, animation frame, idle counter) regenerated each session.
//! The buddy renders as an ASCII sprite that reacts to events and petting.
//! Feature-gated behind `buddy`.

use serde::{Deserialize, Serialize};

/// Buddy personality trait set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BuddyPersonality {
    /// Cheerful and expressive.
    Playful,
    /// Calm and reserved.
    Stoic,
    /// Inquisitive and detail-oriented.
    Curious,
    /// Observant, speaks rarely.
    Quiet,
    /// Supportive and encouraging.
    Encouraging,
}

impl BuddyPersonality {
    /// Default greeting for this personality.
    #[must_use]
    pub fn greeting(&self) -> &'static str {
        match self {
            Self::Playful => "Hey there! Let's code!",
            Self::Stoic => "Ready.",
            Self::Curious => "What shall we explore today?",
            Self::Quiet => "...",
            Self::Encouraging => "You've got this!",
        }
    }
}

/// Buddy's current mood, affects the sprite display.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuddyMood {
    /// Happy after being petted or a success.
    Happy,
    /// Default resting state.
    Neutral,
    /// Processing or working on something.
    Focused,
    /// Idle for too long.
    Sleepy,
    /// Excited by a big win or many pets.
    Excited,
}

impl BuddyMood {
    /// ASCII sprite representation for each mood.
    #[must_use]
    pub fn sprite(&self) -> &'static str {
        match self {
            Self::Happy => "(◕ᴗ◕✿)",
            Self::Neutral => "(・_・)",
            Self::Focused => "(⌐■_■)",
            Self::Sleepy => "(¦ꒉ[▓▓]",
            Self::Excited => "(≧◡≦)",
        }
    }
}

/// Persisted core identity of a buddy, derived from user_id.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuddySoul {
    /// Personality type.
    pub personality: BuddyPersonality,
    /// Generated name.
    pub name: String,
    /// Primary color for theming (0-15 ANSI index).
    pub color_primary: u8,
    /// Secondary color for theming (0-15 ANSI index).
    pub color_secondary: u8,
}

impl BuddySoul {
    /// Deterministically generate a buddy soul from a user ID.
    /// Uses a simple hash to derive personality, name, and colors.
    #[must_use]
    pub fn from_user_id(user_id: &str) -> Self {
        let hash = simple_hash(user_id);
        let personalities = [
            BuddyPersonality::Playful,
            BuddyPersonality::Stoic,
            BuddyPersonality::Curious,
            BuddyPersonality::Quiet,
            BuddyPersonality::Encouraging,
        ];
        let names = [
            "Rex", "Pixel", "Nova", "Byte", "Echo",
            "Luna", "Cosmo", "Dash", "Spark", "Glitch",
        ];

        Self {
            personality: personalities[hash % personalities.len()],
            name: names[(hash / 7) % names.len()].into(),
            color_primary: (hash % 14 + 1) as u8,
            color_secondary: ((hash / 3) % 14 + 1) as u8,
        }
    }

    /// Serialize to JSON string.
    pub fn to_json(&self) -> Result<String, String> {
        serde_json::to_string_pretty(self).map_err(|e| e.to_string())
    }

    /// Deserialize from JSON string.
    pub fn from_json(json: &str) -> Result<Self, String> {
        serde_json::from_str(json).map_err(|e| e.to_string())
    }
}

/// Simple hash of a string for deterministic generation.
fn simple_hash(s: &str) -> usize {
    let mut h: usize = 5381;
    for b in s.bytes() {
        h = h.wrapping_mul(33).wrapping_add(b as usize).wrapping_add(h >> 5);
    }
    h
}

/// Session-local buddy state, regenerated each session.
#[derive(Debug, Clone)]
pub struct BuddyBones {
    /// Current mood.
    pub mood: BuddyMood,
    /// Animation frame index (cycles on idle).
    pub sprite_frame: usize,
    /// Ticks since last interaction.
    pub idle_counter: usize,
}

impl BuddyBones {
    /// Regenerate bones from a soul for a new session.
    #[must_use]
    pub fn regenerate(soul: &BuddySoul) -> Self {
        // Start with mood based on personality
        let mood = match soul.personality {
            BuddyPersonality::Playful => BuddyMood::Happy,
            BuddyPersonality::Stoic => BuddyMood::Neutral,
            BuddyPersonality::Curious => BuddyMood::Focused,
            BuddyPersonality::Quiet => BuddyMood::Neutral,
            BuddyPersonality::Encouraging => BuddyMood::Happy,
        };

        Self {
            mood,
            sprite_frame: 0,
            idle_counter: 0,
        }
    }

    /// Advance idle animation by one tick.
    pub fn idle_tick(&mut self) {
        self.idle_counter += 1;
        if self.idle_counter > 30 && self.mood != BuddyMood::Sleepy {
            self.mood = BuddyMood::Sleepy;
        }
        if self.idle_counter % 5 == 0 {
            self.sprite_frame = (self.sprite_frame + 1) % 3;
        }
    }

    /// Reset idle timer and set mood to happy.
    pub fn interacted(&mut self) {
        self.idle_counter = 0;
        if self.mood != BuddyMood::Excited {
            self.mood = BuddyMood::Happy;
        }
    }
}

/// Companion panel — manages the buddy widget state and reactions.
#[derive(Debug)]
pub struct CompanionPanel {
    /// The buddy's persistent soul.
    pub soul: BuddySoul,
    /// Session-local bones.
    pub bones: BuddyBones,
    /// Total times the buddy has been petted.
    pub pet_count: usize,
    /// Pending reaction messages to display.
    pub reaction_queue: Vec<String>,
}

impl CompanionPanel {
    /// Create a new companion panel from a user ID.
    #[must_use]
    pub fn new(user_id: &str) -> Self {
        let soul = BuddySoul::from_user_id(user_id);
        let bones = BuddyBones::regenerate(&soul);

        Self {
            soul,
            bones,
            pet_count: 0,
            reaction_queue: Vec::new(),
        }
    }

    /// Create from an existing soul (e.g., loaded from disk).
    #[must_use]
    pub fn from_soul(soul: BuddySoul) -> Self {
        let bones = BuddyBones::regenerate(&soul);

        Self {
            soul,
            bones,
            pet_count: 0,
            reaction_queue: Vec::new(),
        }
    }

    /// Pet the buddy.
    pub fn pet(&mut self) {
        self.pet_count += 1;
        self.bones.interacted();
        if self.pet_count % 5 == 0 {
            self.bones.mood = BuddyMood::Excited;
        }
    }

    /// React to an event.
    pub fn react_to(&mut self, event: &BuddyEvent) {
        self.bones.interacted();
        let reaction = match event {
            BuddyEvent::ToolSuccess => "✓ Nice!",
            BuddyEvent::ToolError => "! Hmm...",
            BuddyEvent::UserMessage => "...",
            BuddyEvent::StreamComplete => "~ Done!",
        };
        self.reaction_queue.push(reaction.into());
        if self.reaction_queue.len() > 3 {
            self.reaction_queue.drain(..self.reaction_queue.len() - 3);
        }
    }

    /// Advance the idle animation tick.
    pub fn idle_tick(&mut self) {
        self.bones.idle_tick();
    }

    /// Get the current sprite string.
    #[must_use]
    pub fn render_sprite(&self) -> &str {
        self.bones.mood.sprite()
    }

    /// Get the full display line: sprite + name + reaction.
    #[must_use]
    pub fn render_line(&self) -> String {
        let reaction = self.reaction_queue.last().cloned().unwrap_or_default();
        format!("{} {} {}", self.render_sprite(), self.soul.name, reaction)
    }
}

/// Events that the buddy can react to.
#[derive(Debug, Clone)]
pub enum BuddyEvent {
    /// A tool call succeeded.
    ToolSuccess,
    /// A tool call failed.
    ToolError,
    /// The user sent a message.
    UserMessage,
    /// The assistant stream completed.
    StreamComplete,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_soul_generation() {
        let soul1 = BuddySoul::from_user_id("user_123");
        let soul2 = BuddySoul::from_user_id("user_123");
        assert_eq!(soul1.name, soul2.name);
        assert_eq!(soul1.personality, soul2.personality);
        assert_eq!(soul1.color_primary, soul2.color_primary);
        assert_eq!(soul1.color_secondary, soul2.color_secondary);
    }

    #[test]
    fn different_users_different_souls() {
        // Use distinctly different IDs to avoid hash collision
        let soul1 = BuddySoul::from_user_id("alice_42_dev");
        let soul2 = BuddySoul::from_user_id("bob_99_data_scientist");
        assert_ne!(soul1.name, soul2.name);
    }

    #[test]
    fn bones_regeneration() {
        let soul = BuddySoul::from_user_id("test");
        let bones = BuddyBones::regenerate(&soul);
        assert_eq!(bones.sprite_frame, 0);
        assert_eq!(bones.idle_counter, 0);
        // Mood depends on personality
        assert!(!matches!(bones.mood, BuddyMood::Sleepy));
    }

    #[test]
    fn pet_counter_increments() {
        let mut panel = CompanionPanel::new("test_user");
        assert_eq!(panel.pet_count, 0);
        panel.pet();
        assert_eq!(panel.pet_count, 1);
        panel.pet();
        assert_eq!(panel.pet_count, 2);
    }

    #[test]
    fn mood_transitions() {
        let mut panel = CompanionPanel::new("test");
        panel.pet();
        assert_eq!(panel.bones.mood, BuddyMood::Happy);

        // Pet 5 times triggers excited
        for _ in 0..4 {
            panel.pet();
        }
        assert_eq!(panel.bones.mood, BuddyMood::Excited);
    }

    #[test]
    fn soul_json_roundtrip() {
        let soul = BuddySoul::from_user_id("roundtrip_test");
        let json = soul.to_json().unwrap();
        let restored = BuddySoul::from_json(&json).unwrap();
        assert_eq!(soul.name, restored.name);
        assert_eq!(soul.personality, restored.personality);
    }

    #[test]
    fn idle_tick_cycles_animation() {
        let mut panel = CompanionPanel::new("idle_test");
        // Tick enough times to become sleepy
        for _ in 0..31 {
            panel.idle_tick();
        }
        assert_eq!(panel.bones.mood, BuddyMood::Sleepy);

        // Interact resets
        panel.pet();
        assert!(panel.bones.mood != BuddyMood::Sleepy);
    }

    #[test]
    fn sprite_renders_for_all_moods() {
        for mood in [
            BuddyMood::Happy,
            BuddyMood::Neutral,
            BuddyMood::Focused,
            BuddyMood::Sleepy,
            BuddyMood::Excited,
        ] {
            let sprite = mood.sprite();
            assert!(!sprite.is_empty());
            assert!(sprite.len() > 3);
        }
    }
}
