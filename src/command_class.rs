//! How a slash command relates to an agent turn in flight.
//!
//! Every command declares a [`CommandClass`] where it is registered: the
//! built-in table in `ui::slash`, or an addon's `:dirge/commands` entry. The
//! busy gate asks for the class of what was typed and knows no command names,
//! so a new command never needs a gate edit.

/// What running a slash command does to state a running turn depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CommandClass {
    /// Acts on the display, or on a setting a running turn reads live
    /// (reasoning visibility, permission mode, effort), or quits.
    View,
    /// Reads state and shows it; changes nothing.
    ReadOnly,
    /// Changes conversation, agent or process state a running turn depends
    /// on. The default: a command that declares nothing is gated.
    #[default]
    Mutating,
}

impl CommandClass {
    /// Whether the command may run while the loop is busy with a turn.
    pub fn runs_while_busy(self) -> bool {
        !matches!(self, CommandClass::Mutating)
    }

    /// Parse a declared class: `view`, `read-only` or `mutating`, with or
    /// without a leading `:`. `None` for anything else.
    #[cfg_attr(not(feature = "addons"), allow(dead_code))]
    pub fn parse(declared: &str) -> Option<Self> {
        match declared.trim().trim_start_matches(':') {
            "view" => Some(CommandClass::View),
            "read-only" | "read_only" | "readonly" => Some(CommandClass::ReadOnly),
            "mutating" => Some(CommandClass::Mutating),
            _ => None,
        }
    }
}

/// A first-argument shape a [`ClassRule`] can single out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgForm {
    /// No argument at all.
    Bare,
    /// The first argument is exactly this word.
    Word(&'static str),
}

impl ArgForm {
    fn matches(self, first_arg: Option<&str>) -> bool {
        match self {
            ArgForm::Bare => first_arg.is_none(),
            ArgForm::Word(word) => first_arg == Some(word),
        }
    }
}

/// A command's class as a function of its first argument: the listed
/// `read_only_forms` are [`CommandClass::ReadOnly`], anything else is `base`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClassRule {
    base: CommandClass,
    read_only_forms: &'static [ArgForm],
}

impl ClassRule {
    /// Every invocation has `class`.
    pub const fn always(class: CommandClass) -> Self {
        ClassRule {
            base: class,
            read_only_forms: &[],
        }
    }

    /// Read-only for the listed forms, mutating otherwise.
    pub const fn read_only_when(forms: &'static [ArgForm]) -> Self {
        ClassRule {
            base: CommandClass::Mutating,
            read_only_forms: forms,
        }
    }

    /// The class of one invocation.
    pub fn classify(&self, first_arg: Option<&str>) -> CommandClass {
        if self.read_only_forms.iter().any(|f| f.matches(first_arg)) {
            CommandClass::ReadOnly
        } else {
            self.base
        }
    }

    /// Whether some invocation of the command runs while the loop is busy.
    pub fn ever_runs_while_busy(&self) -> bool {
        self.base.runs_while_busy() || !self.read_only_forms.is_empty()
    }
}

impl From<CommandClass> for ClassRule {
    fn from(class: CommandClass) -> Self {
        ClassRule::always(class)
    }
}

/// The class of the slash line `text`, its head resolved through `lookup`.
/// A command `lookup` does not know is [`CommandClass::Mutating`].
pub fn classify(text: &str, lookup: impl Fn(&str) -> Option<ClassRule>) -> CommandClass {
    let mut words = text.split_whitespace();
    let head = words.next().unwrap_or("");
    let first_arg = words.next();
    lookup(head).map_or(CommandClass::Mutating, |rule| rule.classify(first_arg))
}

#[cfg(test)]
mod tests {
    use super::*;

    const LISTING: ClassRule = ClassRule::read_only_when(&[ArgForm::Bare, ArgForm::Word("list")]);

    fn table(head: &str) -> Option<ClassRule> {
        match head {
            "/help" => Some(ClassRule::always(CommandClass::ReadOnly)),
            "/mode" => Some(CommandClass::View.into()),
            "/sessions" => Some(LISTING),
            "/cd" => Some(ClassRule::always(CommandClass::Mutating)),
            _ => None,
        }
    }

    #[test]
    fn only_mutating_is_gated() {
        assert!(CommandClass::View.runs_while_busy());
        assert!(CommandClass::ReadOnly.runs_while_busy());
        assert!(!CommandClass::Mutating.runs_while_busy());
        assert_eq!(CommandClass::default(), CommandClass::Mutating);
    }

    #[test]
    fn a_rule_reads_only_the_first_argument() {
        assert_eq!(classify("/sessions", table), CommandClass::ReadOnly);
        assert_eq!(classify("/sessions list", table), CommandClass::ReadOnly);
        assert_eq!(classify("/sessions list 3", table), CommandClass::ReadOnly);
        assert_eq!(classify("/sessions  list", table), CommandClass::ReadOnly);
        assert_eq!(classify("/sessions 42", table), CommandClass::Mutating);
        assert_eq!(classify("/mode yolo", table), CommandClass::View);
        assert_eq!(classify("/help me", table), CommandClass::ReadOnly);
        assert_eq!(classify("/cd", table), CommandClass::Mutating);
    }

    #[test]
    fn an_unknown_command_or_empty_line_is_mutating() {
        assert_eq!(classify("/nope", table), CommandClass::Mutating);
        assert_eq!(classify("", table), CommandClass::Mutating);
        assert_eq!(classify("   ", table), CommandClass::Mutating);
    }

    #[test]
    fn ever_runs_while_busy_sees_any_safe_form() {
        assert!(LISTING.ever_runs_while_busy());
        assert!(ClassRule::always(CommandClass::View).ever_runs_while_busy());
        assert!(!ClassRule::always(CommandClass::Mutating).ever_runs_while_busy());
    }

    #[test]
    fn declared_classes_parse_and_anything_else_is_refused() {
        assert_eq!(CommandClass::parse("view"), Some(CommandClass::View));
        assert_eq!(
            CommandClass::parse(":read-only"),
            Some(CommandClass::ReadOnly)
        );
        assert_eq!(
            CommandClass::parse("read_only"),
            Some(CommandClass::ReadOnly)
        );
        assert_eq!(
            CommandClass::parse(" mutating "),
            Some(CommandClass::Mutating)
        );
        assert_eq!(CommandClass::parse("safe"), None);
        assert_eq!(CommandClass::parse(""), None);
    }
}
