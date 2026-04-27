/// Per-layer compositing blend mode.
///
/// One render pipeline per variant lives in the compositor; the selection
/// is made by `pipeline_for(BlendMode)`. `Overlay` is intentionally absent
/// — it needs a custom shader rather than a fixed-function blend state, so
/// it'll land alongside the effects pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[derive(Default)]
pub enum BlendMode {
    #[default]
    Normal,
    Add,
    Multiply,
    Screen,
}

impl BlendMode {
    pub const ALL: &'static [BlendMode] = &[
        BlendMode::Normal,
        BlendMode::Add,
        BlendMode::Multiply,
        BlendMode::Screen,
    ];

    pub fn label(self) -> &'static str {
        match self {
            BlendMode::Normal => "Normal",
            BlendMode::Add => "Add",
            BlendMode::Multiply => "Multiply",
            BlendMode::Screen => "Screen",
        }
    }
}


#[cfg(test)]
mod tests {
    use super::BlendMode;

    #[test]
    fn all_covers_every_variant() {
        // Tighten this test if a new variant is added — it must appear in ALL.
        assert_eq!(BlendMode::ALL.len(), 4);
        assert!(BlendMode::ALL.contains(&BlendMode::Normal));
        assert!(BlendMode::ALL.contains(&BlendMode::Add));
        assert!(BlendMode::ALL.contains(&BlendMode::Multiply));
        assert!(BlendMode::ALL.contains(&BlendMode::Screen));
    }

    #[test]
    fn labels_are_human_readable() {
        assert_eq!(BlendMode::Normal.label(), "Normal");
        assert_eq!(BlendMode::Add.label(), "Add");
        assert_eq!(BlendMode::Multiply.label(), "Multiply");
        assert_eq!(BlendMode::Screen.label(), "Screen");
    }

    #[test]
    fn default_is_normal() {
        assert_eq!(BlendMode::default(), BlendMode::Normal);
    }
}
