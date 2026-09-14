//! Command line parsing. Two flags don't justify a dependency.

/// What to do with the arguments we were given.
pub enum Invocation {
    Run(Options),
    Help,
    Version,
    /// The message explains what was wrong.
    Invalid(String),
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Options {
    /// Quick capture: a floating panel that records the moment it opens and
    /// puts the transcript on the clipboard as it closes.
    pub quick: bool,
}

pub const USAGE: &str = "\
yapper — push-to-talk speech-to-text for Wayland

USAGE:
    yapper [OPTIONS]

OPTIONS:
    -q, --quick      Quick capture: open as a floating panel, start recording
                     immediately, and copy the transcript to the clipboard on
                     close. Meant to be bound to a key.
    -h, --help       Print this help
    -V, --version    Print the version

While running:
    Ctrl+Space       Start or stop recording
    Escape           Stop recording (quick capture: stop, copy and close)
    Enter            Copy the selected recording
    Delete           Delete the selected recording

    pkill -USR1 yapper toggles recording without the window being focused.
";

pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Invocation {
    let mut options = Options::default();

    for argument in args {
        match argument.as_str() {
            "-q" | "--quick" => options.quick = true,
            "-h" | "--help" => return Invocation::Help,
            "-V" | "--version" => return Invocation::Version,
            other => {
                return Invocation::Invalid(format!("unrecognised argument: {other}"));
            }
        }
    }

    Invocation::Run(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_args(args: &[&str]) -> Invocation {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn no_arguments_runs_the_normal_window() {
        assert!(matches!(
            parse_args(&[]),
            Invocation::Run(Options { quick: false })
        ));
    }

    #[test]
    fn quick_has_a_short_and_a_long_form() {
        assert!(matches!(
            parse_args(&["-q"]),
            Invocation::Run(Options { quick: true })
        ));
        assert!(matches!(
            parse_args(&["--quick"]),
            Invocation::Run(Options { quick: true })
        ));
    }

    #[test]
    fn help_and_version_win_over_everything_else() {
        assert!(matches!(parse_args(&["--quick", "--help"]), Invocation::Help));
        assert!(matches!(parse_args(&["-V"]), Invocation::Version));
    }

    #[test]
    fn an_unknown_argument_is_reported_rather_than_ignored() {
        let Invocation::Invalid(message) = parse_args(&["--turbo"]) else {
            panic!("expected an error");
        };
        assert!(message.contains("--turbo"), "{message}");
    }
}
