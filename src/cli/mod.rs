//! A CLI11-compatible command-line parser — the subset of CLI11 2.x that wally
//! used, reproduced so help text, error sentences and exit codes stay
//! byte-identical to the C++ build (tests/golden/ is the proof). Replaces
//! third_party/CLI11.
//!
//! The builder mirrors the CLI11 calls the C++ made, one for one:
//!
//! | CLI11 (C++)                                  | here                                        |
//! |----------------------------------------------|---------------------------------------------|
//! | `app.add_subcommand("pull", "Download…")`    | `app.add_subcommand("pull", "Download…")`   |
//! | `sub->alias("download")`                     | `.alias("download")`                        |
//! | `sub->group("")` (hidden)                    | `.group("")`                                |
//! | `cmd->add_option("--top-k", opts->top_k, d)` | `.add_option("--top-k", ValueType::Int, d)` |
//! | `cmd->add_option("--stop", vec, d)`          | `.add_option(..).multi()`                   |
//! | `cmd->add_option("model", model, d)`         | same — a name without dashes is positional  |
//! | `cmd->add_flag("--json", b, d)`              | `.add_flag("--json", d)`                    |
//! | `->required()`, `->default_val(x)`           | `.required()`, `.default_val("x")`          |
//! | `->check(CLI::ExistingFile)`                 | `.check(Validator::ExistingFile)`           |
//! | `->group("Sampling")`                        | `.group("Sampling")`                        |
//! | `cmd->callback([&]{…})`                      | `.callback(\|p, g\| …)` returning exit code  |
//!
//! Where C++ bound an option to a variable, Rust reads it back in the callback
//! by any of its names: `p.get_i64("--top-k")`, `p.get_str("model")`,
//! `p.flag("--json")`. Values are converted and validated during the parse, so
//! a bad value fails with CLI11's message and exit code 2 before any callback.
//! A callback returns the process exit code (C++ threw CLI::RuntimeError(code)).
//!
//! Owner of the parser/help/callback internals: the CLI port. Command ports only
//! use the builder and `Parsed`.

use std::collections::BTreeMap;
use std::rc::Rc;

use crate::bootstrap::GlobalOptions;

/// Splits a CLI11 name spec (`"--model,-m"`, `"model"`) into option names and
/// (at most one) positional name, the way `CLI::detail::split_names` /
/// `App::_add_option`'s name parsing does.
fn split_name_spec(spec: &str) -> (Vec<String>, String) {
    let mut names = Vec::new();
    let mut positional = String::new();
    for token in spec.split(',') {
        let token = token.trim();
        if token.is_empty() {
            continue;
        }
        if token.starts_with('-') {
            names.push(token.to_string());
        } else {
            positional = token.to_string();
        }
    }
    (names, positional)
}

/// Splits one flag-spec token (`"--hide-thinking{false}"`) into its name and
/// the result CLI11 records when that name is given: the brace contents, or
/// `"true"` for a plain flag name.
fn split_flag_value(token: &str) -> (String, String) {
    if let Some(brace) = token.find('{') {
        if token.ends_with('}') {
            return (
                token[..brace].to_string(),
                token[brace + 1..token.len() - 1].to_string(),
            );
        }
    }
    (token.to_string(), "true".to_string())
}

/// The value type an option was bound to in C++; decides conversion, the help
/// type name (`TEXT`, `INT`, `UINT`, `FLOAT`) and CLI11's conversion errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValueType {
    /// std::string
    #[default]
    Text,
    /// int / int32_t
    Int,
    /// int64_t / long long
    Int64,
    /// unsigned / uint32_t
    UInt,
    /// uint64_t / size_t
    UInt64,
    /// float
    Float,
    /// double
    Double,
}

/// The CLI11 validators wally attached with `->check(...)`.
#[derive(Debug, Clone, PartialEq)]
pub enum Validator {
    /// CLI::ExistingFile — help type name becomes `FILE`.
    ExistingFile,
    /// CLI::Range(min, max) on an integer option.
    Range(i64, i64),
    /// CLI::Range(min, max) on a floating option.
    RangeF(f64, f64),
    /// CLI::PositiveNumber — help type name becomes `POSITIVE`.
    PositiveNumber,
    /// CLI::NonNegativeNumber — help type name becomes `NONNEGATIVE`.
    NonNegativeNumber,
    /// CLI::IsMember({...}) — help type name becomes `{a,b,c}`.
    IsMember(Vec<String>),
}

/// The callback a (sub)command runs after a successful parse.
pub type Callback = Rc<dyn Fn(&Parsed, &GlobalOptions) -> i32>;

/// One option, flag or positional, as registered.
#[derive(Clone, Debug, Default)]
pub struct Opt {
    /// The CLI11 name string as registered, e.g. `"--model,-m"`,
    /// `"--show-thinking,--hide-thinking{false}"`, or `"model"`.
    pub spec: String,
    /// Short (`-m`) and long (`--model`) names, in registration order, without
    /// `{default}` suffixes. Empty for a positional.
    pub names: Vec<String>,
    /// Positional name (`model`), empty for options and flags.
    pub positional: String,
    pub is_flag: bool,
    pub value_type: ValueType,
    /// A vector option/positional (`std::vector<...>`): repeatable / greedy.
    pub multi: bool,
    pub description: String,
    /// Help section ("Options" when empty; "Sampling", …).
    pub group: String,
    pub required: bool,
    pub default_value: Option<String>,
    pub validators: Vec<Validator>,
    /// Explicit `->type_name("...")` override.
    pub type_name: Option<String>,
    /// Per-name flag values from `{...}` suffixes (`--hide-thinking{false}`).
    pub flag_values: BTreeMap<String, String>,
    /// Overrides `integer_bounds(value_type)`'s default `[min, max]` for an
    /// option whose bound C++ variable is narrower than any `ValueType`
    /// variant distinguishes (e.g. `serve --port`'s `uint16_t`). Kept as a
    /// separate field rather than a new `ValueType` variant because
    /// `ValueType` is matched exhaustively by `cli_formatter.rs`, owned by a
    /// different worker.
    pub int_bound_override: Option<(i128, i128)>,
}

impl Opt {
    pub fn required(&mut self) -> &mut Self {
        self.required = true;
        self
    }
    pub fn default_val(&mut self, value: &str) -> &mut Self {
        self.default_value = Some(value.to_string());
        self
    }
    pub fn check(&mut self, validator: Validator) -> &mut Self {
        self.validators.push(validator);
        self
    }
    pub fn group(&mut self, group: &str) -> &mut Self {
        self.group = group.to_string();
        self
    }
    pub fn type_name(&mut self, name: &str) -> &mut Self {
        self.type_name = Some(name.to_string());
        self
    }
    /// Narrows the integer conversion/range check to `[min, max]`, for an
    /// option bound to a C++ integer width no `ValueType` variant covers
    /// (e.g. `serve --port`'s `uint16_t`, `[0, 65535]`).
    pub fn int_bounds(&mut self, min: i128, max: i128) -> &mut Self {
        self.int_bound_override = Some((min, max));
        self
    }
    /// Bound to a `std::vector` in C++: may repeat (option) / takes the rest (positional).
    pub fn multi(&mut self) -> &mut Self {
        self.multi = true;
        self
    }
    /// True when `name` is one of this option's names (or its positional name).
    pub fn matches(&self, name: &str) -> bool {
        (!self.positional.is_empty() && self.positional == name)
            || self.names.iter().any(|n| n == name)
    }
}

/// A (sub)command.
#[derive(Clone, Default)]
pub struct App {
    pub name: String,
    pub description: String,
    pub footer: String,
    /// CLI11 group. For a subcommand, "" hides it from its parent's help.
    pub group: String,
    pub aliases: Vec<String>,
    pub subcommands: Vec<App>,
    pub options: Vec<Opt>,
    pub callback: Option<Callback>,
    pub require_subcommand_min: usize,
    /// 0 means "no maximum" (CLI11's convention).
    pub require_subcommand_max: usize,
    pub fallthrough: bool,
    pub prefix_command: bool,
    pub allow_extras: bool,
    /// `set_help_flag` names and description; inherited by subcommands.
    pub help_flag: Option<(String, String)>,
    /// `set_version_flag` names, version text and description.
    pub version_flag: Option<(String, String, String)>,
}

impl App {
    /// `CLI::App app{description, name}`.
    pub fn new(description: &str, name: &str) -> App {
        App {
            name: name.to_string(),
            description: description.to_string(),
            group: "SUBCOMMANDS".into(),
            ..App::default()
        }
    }

    /// `add_subcommand(name, description)`. The new subcommand inherits what
    /// CLI11 copies from its parent at construction (help flag, fallthrough, …).
    ///
    /// Mirrors CLI11's `App::App(description, name, parent)` constructor
    /// (CLI11.hpp): help flag, `allow_extras`, `prefix_command`, `fallthrough`,
    /// `group` and `require_subcommand_max` are copied from the parent at the
    /// moment the child is added; `require_subcommand_min` and `version_flag`
    /// are never inherited.
    pub fn add_subcommand(&mut self, name: &str, description: &str) -> &mut App {
        let mut sub = App::new(description, name);
        sub.help_flag = self.help_flag.clone();
        sub.allow_extras = self.allow_extras;
        sub.prefix_command = self.prefix_command;
        sub.fallthrough = self.fallthrough;
        sub.group = self.group.clone();
        sub.footer = self.footer.clone();
        sub.require_subcommand_max = self.require_subcommand_max;
        self.subcommands.push(sub);
        self.subcommands.last_mut().expect("just pushed")
    }

    pub fn get_subcommand(&self, name: &str) -> Option<&App> {
        self.subcommands
            .iter()
            .find(|s| s.name == name || s.aliases.iter().any(|a| a == name))
    }

    pub fn get_subcommand_mut(&mut self, name: &str) -> Option<&mut App> {
        self.subcommands
            .iter_mut()
            .find(|s| s.name == name || s.aliases.iter().any(|a| a == name))
    }

    pub fn alias(&mut self, name: &str) -> &mut Self {
        self.aliases.push(name.to_string());
        self
    }
    pub fn group(&mut self, group: &str) -> &mut Self {
        self.group = group.to_string();
        self
    }
    pub fn footer(&mut self, footer: &str) -> &mut Self {
        self.footer = footer.to_string();
        self
    }
    pub fn description(&mut self, description: &str) -> &mut Self {
        self.description = description.to_string();
        self
    }
    /// `require_subcommand(min, max)`; `require_subcommand()` is `(1, 0)`.
    pub fn require_subcommand(&mut self, min: usize, max: usize) -> &mut Self {
        self.require_subcommand_min = min;
        self.require_subcommand_max = max;
        self
    }
    pub fn fallthrough(&mut self, on: bool) -> &mut Self {
        self.fallthrough = on;
        self
    }
    pub fn prefix_command(&mut self, on: bool) -> &mut Self {
        self.prefix_command = on;
        self
    }
    pub fn allow_extras(&mut self, on: bool) -> &mut Self {
        self.allow_extras = on;
        self
    }
    pub fn set_help_flag(&mut self, names: &str, description: &str) -> &mut Self {
        self.help_flag = Some((names.to_string(), description.to_string()));
        self
    }
    pub fn set_version_flag(&mut self, names: &str, version: &str, description: &str) -> &mut Self {
        self.version_flag = Some((
            names.to_string(),
            version.to_string(),
            description.to_string(),
        ));
        self
    }

    /// `add_option(spec, variable, description)`. A spec without a leading
    /// dash (`"model"`) is a positional.
    pub fn add_option(&mut self, spec: &str, value_type: ValueType, description: &str) -> &mut Opt {
        let (names, positional) = split_name_spec(spec);
        self.options.push(Opt {
            spec: spec.to_string(),
            names,
            positional,
            is_flag: false,
            value_type,
            multi: false,
            description: description.to_string(),
            group: String::new(),
            required: false,
            default_value: None,
            validators: Vec::new(),
            type_name: None,
            flag_values: BTreeMap::new(),
            int_bound_override: None,
        });
        self.options.last_mut().expect("just pushed")
    }

    /// `add_flag(spec, variable, description)`, including CLI11's
    /// `--on,--off{false}` value suffixes.
    pub fn add_flag(&mut self, spec: &str, description: &str) -> &mut Opt {
        let mut names = Vec::new();
        let mut positional = String::new();
        let mut flag_values = BTreeMap::new();
        for token in spec.split(',') {
            let token = token.trim();
            if token.is_empty() {
                continue;
            }
            let (name, value) = split_flag_value(token);
            if name.starts_with('-') {
                flag_values.insert(name.clone(), value);
                names.push(name);
            } else {
                positional = name;
            }
        }
        self.options.push(Opt {
            spec: spec.to_string(),
            names,
            positional,
            is_flag: true,
            value_type: ValueType::Text,
            multi: false,
            description: description.to_string(),
            group: String::new(),
            required: false,
            default_value: None,
            validators: Vec::new(),
            type_name: None,
            flag_values,
            int_bound_override: None,
        });
        self.options.last_mut().expect("just pushed")
    }

    pub fn callback(&mut self, f: impl Fn(&Parsed, &GlobalOptions) -> i32 + 'static) -> &mut Self {
        self.callback = Some(Rc::new(f));
        self
    }

    pub fn get_option(&self, name: &str) -> Option<&Opt> {
        self.options.iter().find(|o| o.matches(name))
    }

    pub fn get_option_mut(&mut self, name: &str) -> Option<&mut Opt> {
        self.options.iter_mut().find(|o| o.matches(name))
    }
}

/// What a full parse of an argv slice produced, matching how CLI11's
/// `App::parse` / `App::exit` classify the outcome (app.cpp's `run()` is the
/// full switch over these). `path` is always the chain of subcommand names
/// from (but not including) the root down to the deepest subcommand CLI11
/// actually entered while consuming the tokens — the same chain
/// `App::help(prev)`'s self-accumulating recursion and wally's own
/// `PrintParseErrorHelp` walk to find "the deepest command that actually
/// parsed" — regardless of which level in that chain raised the error.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The deepest matched command's callback ran (CLI11's bottom-up
    /// `run_callback()`, after `_process()` and `_process_extras()` both
    /// succeeded); carries its exit code and the subcommand chain entered
    /// (empty for a bare `wally` with no subcommand at all — app.rs prints
    /// the root help to stderr for that case, C++'s `out::status_line(app.help())`).
    Ran { code: i32, path: Vec<String> },
    /// `-h`/`--help` matched anywhere in the chain (`_process_help_flags`
    /// takes precedence over every other outcome, including a later required
    /// or extra argument).
    Help { path: Vec<String> },
    /// `-V`/`--version` matched; `text` is `CLI::CallForVersion::what()`
    /// (`"wally " + WALLY_VERSION`).
    Version { text: String },
    /// `CLI::RequiredError`: a required (sub)command or option was left out.
    Required { message: String, path: Vec<String> },
    /// `CLI::ExtrasError`: an unexpected or misspelled (sub)command/argument.
    Extras { message: String, path: Vec<String> },
    /// Any other `CLI::ParseError` (bad type, a failed `->check()`, wrong
    /// argument count): reported via CLI11's `FailureMessage::simple`, with
    /// no help block.
    ParseErr { message: String },
}

impl App {
    /// `CLI::App::parse(argc, argv)` plus the `run()` switch over what it
    /// threw — everything cli/mod.rs owns of CLI11's behaviour: token
    /// classification, subcommand descent, option fallthrough, positional
    /// filling, conversion/validation, requirements-before-extras, and
    /// (unlike CLI11 itself) actually invoking the matched callback chain.
    /// `args` excludes the program name.
    pub fn parse(&self, args: &[String]) -> Outcome {
        let mut state = ParseState::new(self);
        state.run(args);
        state.finish()
    }

    /// The deepest command reached along `path` (root if `path` is empty).
    pub fn resolve_path(&self, path: &[String]) -> &App {
        let mut app = self;
        for name in path {
            app = app.get_subcommand(name).unwrap_or(app);
        }
        app
    }

    /// `CliFormatter::make_help` for the command at `path`, with `path`'s
    /// ancestors reconstructed as CLI11's own `App::help(prev)` would
    /// accumulate them (`"wally models"` above `"wally models pull"`).
    pub fn render_help(&self, path: &[String], color_enabled: bool) -> String {
        let mut app = self;
        let mut parents = String::new();
        for name in path {
            parents = if parents.is_empty() {
                app.name.clone()
            } else {
                format!("{parents} {}", app.name)
            };
            app = app.get_subcommand(name).unwrap_or(app);
        }
        crate::cli_formatter::make_help(app, &parents, color_enabled)
    }
}

/// One frame of the parse: the app at this depth, what it has collected so
/// far, and where positional-filling is up to. `positional_only` is CLI11's
/// per-app flag (set by a bare `--`, or by `prefix_command` sweeping the
/// rest of the line): once set, every further token is NONE-classified —
/// never re-tried as a subcommand or option.
struct Frame<'a> {
    app: &'a App,
    parsed: Parsed,
    positional_cursor: usize,
    positional_only: bool,
}

/// The whole in-progress parse: one `Frame` per level of the subcommand chain
/// actually entered, deepest last.
struct ParseState<'a> {
    frames: Vec<Frame<'a>>,
    help_seen: bool,
    version_text: Option<String>,
}

/// A `-x`/`--x`-shaped token, split the way CLI11's `App::_recognize` /
/// `detail::split_long`/`split_short` do.
enum Token<'a> {
    /// `--name` or `--name=value`.
    Long {
        name: &'a str,
        inline: Option<&'a str>,
    },
    /// `-x` (one char) possibly followed by an inline value or more clustered
    /// short flags (`-xvalue`, `-ab`).
    Short { name: &'a str, rest: &'a str },
    /// Not option-shaped: a subcommand name, positional value, or (once
    /// `positional_only`) anything at all.
    Plain,
}

fn classify(token: &str) -> Token<'_> {
    if token == "--" || token == "-" || !token.starts_with('-') {
        return Token::Plain;
    }
    if let Some(rest) = token.strip_prefix("--") {
        return match rest.split_once('=') {
            Some((name, value)) => Token::Long {
                name: &token[..2 + name.len()],
                inline: Some(value),
            },
            None => Token::Long {
                name: token,
                inline: None,
            },
        };
    }
    // Short option: `-x` is the name, whatever follows (`value` or clustered
    // flags) is `rest`. A `-`-prefixed negative number with no matching short
    // option falls back to Plain by the caller when nothing resolves it.
    Token::Short {
        name: &token[..2],
        rest: &token[2..],
    }
}

impl<'a> ParseState<'a> {
    fn new(root: &'a App) -> Self {
        ParseState {
            frames: vec![Frame {
                app: root,
                parsed: Parsed {
                    name: root.name.clone(),
                    ..Parsed::default()
                },
                positional_cursor: 0,
                positional_only: false,
            }],
            help_seen: false,
            version_text: None,
        }
        .with_name_index(root)
    }

    fn with_name_index(mut self, root: &'a App) -> Self {
        index_names(root, &mut self.frames[0].parsed);
        self
    }

    fn path(&self) -> Vec<String> {
        self.frames[1..]
            .iter()
            .map(|f| f.app.name.clone())
            .collect()
    }

    fn run(&mut self, args: &[String]) {
        let mut i = 0usize;
        while i < args.len() {
            let token = args[i].as_str();
            let depth = self.frames.len() - 1;
            let positional_only = self.frames[depth].positional_only;

            if !positional_only && token == "--" {
                self.frames[depth].positional_only = true;
                i += 1;
                continue;
            }

            if !positional_only {
                if let Some(sub) = self.frames[depth].app.get_subcommand(token) {
                    let mut parsed = Parsed {
                        name: sub.name.clone(),
                        ..Parsed::default()
                    };
                    index_names(sub, &mut parsed);
                    self.frames.push(Frame {
                        app: sub,
                        parsed,
                        positional_cursor: 0,
                        positional_only: false,
                    });
                    i += 1;
                    continue;
                }

                match classify(token) {
                    Token::Plain => {}
                    Token::Long { name, inline } => {
                        i = self.consume_option(args, i, name, inline);
                        continue;
                    }
                    Token::Short { name, rest } => {
                        // CLI11's own carve-out (`App::_recognize`): a
                        // short-shaped token whose name character is a digit
                        // is only "recognized" as a short option if that
                        // exact `-N` is registered on *this* frame's own app
                        // (no fallthrough); otherwise it's a negative-number-
                        // shaped plain value (`-1`, `-1.5`, `-99999999999`),
                        // taken as-is rather than reported as an unmatched
                        // option.
                        let is_digit_name = name.as_bytes().get(1).is_some_and(u8::is_ascii_digit);
                        let registered = self.frames[depth]
                            .app
                            .options
                            .iter()
                            .any(|o| o.names.iter().any(|n| n == name));
                        if !is_digit_name || registered {
                            i = self.consume_short(args, i, name, rest);
                            continue;
                        }
                    }
                }
            }

            self.fill_positional(depth, token);
            i += 1;
        }
    }

    /// Tries `name` (with an optional inline `=value`) against the help/
    /// version flags and options reachable by fallthrough from the deepest
    /// frame up to the root, deepest first — CLI11's own bubbling order.
    /// Returns the next index to resume scanning from.
    fn consume_option(
        &mut self,
        args: &[String],
        i: usize,
        name: &str,
        inline: Option<&str>,
    ) -> usize {
        let depth = self.frames.len() - 1;
        for level in (0..=depth).rev() {
            if self.frames[level]
                .app
                .help_flag
                .as_ref()
                .is_some_and(|(names, _)| names.split(',').any(|n| n.trim() == name))
            {
                self.help_seen = true;
                return i + 1;
            }
            if let Some((_, version, _)) = &self.frames[level].app.version_flag {
                if self.frames[level]
                    .app
                    .version_flag
                    .as_ref()
                    .unwrap()
                    .0
                    .split(',')
                    .any(|n| n.trim() == name)
                {
                    self.version_text = Some(version.clone());
                    return i + 1;
                }
            }
            if let Some(opt_idx) = self.frames[level]
                .app
                .options
                .iter()
                .position(|o| o.names.iter().any(|n| n == name))
            {
                return self.consume_matched(args, i, level, opt_idx, inline);
            }
        }
        self.handle_unmatched(depth, args[i].clone(), i)
    }

    fn consume_short(&mut self, args: &[String], i: usize, name: &str, rest: &str) -> usize {
        let depth = self.frames.len() - 1;
        for level in (0..=depth).rev() {
            if self.frames[level]
                .app
                .help_flag
                .as_ref()
                .is_some_and(|(names, _)| names.split(',').any(|n| n.trim() == name))
            {
                self.help_seen = true;
                return if rest.is_empty() {
                    i + 1
                } else {
                    // A clustered short flag after `-h` (e.g. `-hv`): re-scan
                    // the remainder as its own short token.
                    self.consume_short(args, i, &format!("-{}", &rest[..1]), &rest[1..])
                };
            }
            if self.frames[level]
                .app
                .version_flag
                .as_ref()
                .is_some_and(|(names, _, _)| names.split(',').any(|n| n.trim() == name))
            {
                self.version_text = Some(
                    self.frames[level]
                        .app
                        .version_flag
                        .as_ref()
                        .unwrap()
                        .1
                        .clone(),
                );
                return i + 1;
            }
            if let Some(opt_idx) = self.frames[level]
                .app
                .options
                .iter()
                .position(|o| o.names.iter().any(|n| n == name))
            {
                let inline = if rest.is_empty() { None } else { Some(rest) };
                return self.consume_matched(args, i, level, opt_idx, inline);
            }
        }
        self.handle_unmatched(depth, args[i].clone(), i)
    }

    /// Records one match of `app.options[opt_idx]` at frame `level`: a flag
    /// (records its `{value}` result) or an option (consumes exactly one
    /// value — CLI11's `type_size(1, 1)` for a `std::vector<std::string>`-bound
    /// option; a `multi` option accumulates across *repeated* occurrences of
    /// the flag, not by greedily eating multiple tokens in one occurrence).
    fn consume_matched(
        &mut self,
        args: &[String],
        i: usize,
        level: usize,
        opt_idx: usize,
        inline: Option<&str>,
    ) -> usize {
        let is_flag = self.frames[level].app.options[opt_idx].is_flag;
        if is_flag {
            // A flag always consumes only its own token (never a following
            // token, cluster remainder, or short-form `=value` — CLI11's
            // `max_num == 0` branch in `_parse_arg` only ever looks at the
            // *long*-form inline `value`, never `rest`).
            let opt = &self.frames[level].app.options[opt_idx];
            let typed_name = opt
                .names
                .iter()
                .find(|n| args[i].starts_with(n.as_str()))
                .cloned()
                .unwrap_or_default();
            let resolved = match inline {
                // Plain `--flag` (no `=value`) or a short-form match (whose
                // `rest`, if any, was never split on `=` to begin with —
                // see `consume_short`): CLI11's `get_flag_value` returns the
                // typed name's registered `{value}` result if it has one,
                // else the literal `"true"`. None of wally's C++ flags
                // register a `{value}` mapping (`default_flag_values_`
                // stays empty for every one, confirmed by grep), so this is
                // always just `split_flag_value`'s own default/`{false}`
                // text.
                // `--flag=` (long form, empty inline): `get_flag_value`
                // treats an empty `value` the same as no `=` at all —
                // `input_value.empty()` short-circuits to the default/`true`
                // text before `to_flag_value` is ever called.
                None | Some("") => Ok(opt
                    .flag_values
                    .get(&typed_name)
                    .cloned()
                    .unwrap_or_else(|| "true".to_string())),
                // `--flag=value` (long form only — `args[i]` starts with
                // `--`): `get_flag_value` returns `value` verbatim (again,
                // no wally flag has a `{value}` mapping to intercept it),
                // and the bound `bool`'s own `lexical_cast` — CLI11's
                // `to_flag_value` — decides the stored result.
                Some(v) if args[i].starts_with("--") => match flag_value_bool(v) {
                    Some(true) => Ok("true".to_string()),
                    Some(false) => Ok("false".to_string()),
                    None => Err(format!("Could not convert: {typed_name} = {v}")),
                },
                // A short-clustered flag's glued-on remainder (`-fx`): not
                // exercised by any known fuzzed shape; keep the prior,
                // pre-existing behavior (ignore it, same as a bare flag)
                // rather than guess at CLI11's short-form `=` handling.
                Some(_) => Ok(opt
                    .flag_values
                    .get(&typed_name)
                    .cloned()
                    .unwrap_or_else(|| "true".to_string())),
            };
            match resolved {
                Ok(value) => {
                    let spec = opt.spec.clone();
                    self.frames[level]
                        .parsed
                        .flag_values
                        .entry(spec)
                        .or_default()
                        .push(value);
                }
                Err(msg) => self.push_parse_error(msg),
            }
            return i + 1;
        }

        let spec = self.frames[level].app.options[opt_idx].spec.clone();
        let value_type = self.frames[level].app.options[opt_idx].value_type;
        let validators = self.frames[level].app.options[opt_idx].validators.clone();
        let type_name = self.frames[level].app.options[opt_idx].type_name.clone();
        let int_bound_override = self.frames[level].app.options[opt_idx].int_bound_override;
        let display = self.frames[level].app.options[opt_idx]
            .names
            .iter()
            .find(|n| n.starts_with("--"))
            .or_else(|| self.frames[level].app.options[opt_idx].names.first())
            .cloned()
            .unwrap_or_default();

        // Every wally vector-bound option (`--stop`, `--doc,-d`, `--file,-f`,
        // `--text,-t`, ...) is `std::vector<std::string>`, and CLI11 explicitly
        // excludes `std::string` from `is_mutable_container` (it's constructible
        // from a single string), so `type_size(1, 1)` applies: each *occurrence*
        // of the flag consumes exactly one following token, never a greedy run —
        // repetition is how the vector accumulates (`expected_count` is
        // unbounded), not multi-token consumption per occurrence. `multi` here
        // only distinguishes "may repeat" for downstream accumulation semantics;
        // it must not change how many tokens a single occurrence consumes. (The
        // one CLI11 mechanism that *does* make a single occurrence greedy,
        // `->allow_extra_args()`, is applied in wally only to the "args"
        // passthrough positional — see `fill_positional`, unaffected by this.)
        let mut next = i + 1;
        let mut values: Vec<String> = Vec::new();
        // An inline `=value` only counts as "the value" when it's non-empty
        // (CLI11's `_parse_arg`: `} else if(!value.empty()) { // --this=value`
        // — a bare `--opt=` falls straight through to the same mandatory-
        // token consumption a plain `--opt` with nothing after it would
        // use). That consumption is unconditional on the next token's shape:
        // CLI11's own loop (`while(min_num > collected && !args.empty())`)
        // has no classification check at all — an option-shaped or
        // negative-number-shaped next token is still grabbed as the value,
        // never left for later parsing (only a truly *absent* next token
        // raises "N required TYPE missing").
        match inline {
            Some(v) if !v.is_empty() => {
                values.push(v.to_string());
            }
            _ => match args.get(next) {
                Some(v) => {
                    values.push(v.clone());
                    next += 1;
                }
                None => {
                    self.push_parse_error(format!(
                        "{display}: 1 required {} missing",
                        full_type_name(value_type, type_name.as_deref(), &validators)
                    ));
                    return next;
                }
            },
        }

        let mut normalized: Vec<String> = Vec::with_capacity(values.len());
        for raw in &values {
            match validate_and_convert(&display, raw, value_type, &validators, int_bound_override) {
                Ok(canonical) => normalized.push(canonical),
                Err(msg) => {
                    self.push_parse_error(msg);
                    return next;
                }
            }
        }
        self.frames[level]
            .parsed
            .values
            .entry(spec)
            .or_default()
            .extend(normalized);
        next
    }

    /// An unmatched option-shaped token: swept into the current frame's
    /// greedy positional if it's `prefix_command` (CLI11's own "sweep on
    /// first unmatched" — reachable in practice only when
    /// `split_passthrough_argv` couldn't insert a `--`, since it already
    /// does), otherwise recorded as an extra for `_process_extras()`.
    fn handle_unmatched(&mut self, depth: usize, token: String, i: usize) -> usize {
        if self.frames[depth].app.prefix_command {
            self.frames[depth].positional_only = true;
            self.fill_positional(depth, &token);
        } else {
            self.frames[depth].parsed.remaining.push(token);
        }
        i + 1
    }

    fn fill_positional(&mut self, depth: usize, token: &str) {
        let positional_only = self.frames[depth].positional_only;
        // CLI11's `App::_parse_positional` tail fallback: once a bare `--`
        // has set `positional_only`, a NONE-classified token that names one
        // of *this* (sub)command's own subcommands is handed to that
        // subcommand's `_parse` directly (`_find_subcommand` + `com->_parse`)
        // rather than through the normal `SUBCOMMAND` classification —
        // bypassing the bookkeeping (`_parse_subcommand`) that would
        // register it in `parsed_subcommands_`. None of wally's subcommands
        // set `parse_complete_callback_`, so that back-door `_parse` is
        // observably a no-op: the token vanishes instead of becoming an
        // extra, and this (sub)command's own required-subcommand check still
        // sees nothing entered (reproduced against the real C++ binary:
        // `wally -- version` / `-- about` / `--json -- version` all print the
        // bare root help and exit 0, the same as no subcommand at all).
        let swallow_subcommand =
            positional_only && self.frames[depth].app.get_subcommand(token).is_some();
        let frame = &mut self.frames[depth];
        let positionals: Vec<usize> = frame
            .app
            .options
            .iter()
            .enumerate()
            .filter(|(_, o)| !o.positional.is_empty())
            .map(|(idx, _)| idx)
            .collect();
        if frame.positional_cursor < positionals.len() {
            let opt_idx = positionals[frame.positional_cursor];
            let opt = &frame.app.options[opt_idx];
            let spec = opt.spec.clone();
            let multi = opt.multi;
            frame
                .parsed
                .values
                .entry(spec)
                .or_default()
                .push(token.to_string());
            if !multi {
                frame.positional_cursor += 1;
            }
        } else if swallow_subcommand {
            // Silently discarded, as in C++ — see the comment above.
        } else {
            // Whether this later gets reported (`_process_extras()`, `finish()`)
            // depends on `prefix_command`/`allow_extras` there, not here — every
            // positional-exhausted token is recorded the same way.
            frame.parsed.remaining.push(token.to_string());
        }
    }

    fn push_parse_error(&mut self, message: String) {
        self.frames
            .last_mut()
            .unwrap()
            .parsed
            .remaining
            .push(format!("\0parse-error\0{message}"));
    }

    fn finish(self) -> Outcome {
        // A bad conversion/argument count is recorded as a sentinel in
        // `remaining` the moment it happens (CLI11 throws immediately, from
        // wherever the bad token was) — surface the first one, in scan order,
        // ahead of anything else.
        for frame in &self.frames {
            if let Some(msg) = frame
                .parsed
                .remaining
                .iter()
                .find_map(|r| r.strip_prefix("\0parse-error\0"))
            {
                return Outcome::ParseErr {
                    message: msg.to_string(),
                };
            }
        }

        // `_process_help_flags()` / `_process_callbacks()` for the version
        // flag both run before requirements or extras are ever checked.
        if self.help_seen {
            return Outcome::Help { path: self.path() };
        }
        if let Some(text) = self.version_text {
            return Outcome::Version { text };
        }

        let path = self.path();

        // `Option::run_callback()` (via `App::_process_callbacks()`, called
        // from `_process()` before `_process_requirements()`/
        // `_process_extras()`): each option's `_reduce_results` applies its
        // `multi_option_policy_`, defaulted to `MultiOptionPolicy::Throw`
        // (CLI11.hpp) — a non-`multi` option given more than once (repeat
        // occurrences all accumulate into the same `results_`, since
        // `expected_max_` is 1 for anything not raised to
        // `detail::expected_max_vector_size`) throws
        // `ArgumentMismatch::AtMost(get_name(), 1, results_.size())`. This
        // runs top-down like requirements/extras, but before both — and
        // after any conversion/missing-value error above, since those throw
        // immediately during the scan, before `_process()` is ever reached.
        for frame in &self.frames {
            for opt in &frame.app.options {
                if opt.multi || opt.is_flag {
                    continue;
                }
                let count = frame.parsed.values.get(&opt.spec).map_or(0, Vec::len);
                if count > 1 {
                    return Outcome::ParseErr {
                        message: format!(
                            "{}: At Most 1 required but received {count}",
                            primary_name(opt)
                        ),
                    };
                }
            }
        }

        // `_process_requirements()`: top-down, first violation wins.
        for (level, frame) in self.frames.iter().enumerate() {
            for opt in &frame.app.options {
                if opt.required && frame.parsed.count(primary_name(opt)) == 0 {
                    return Outcome::Required {
                        message: format!("{} is required", primary_name(opt)),
                        path: path.clone(),
                    };
                }
            }
            if frame.app.require_subcommand_min > 0 {
                let has_child = self.frames.get(level + 1).is_some();
                if !has_child {
                    let message = if frame.app.require_subcommand_min == 1 {
                        "A subcommand is required".to_string()
                    } else {
                        format!(
                            "Requires at least {} subcommands",
                            frame.app.require_subcommand_min
                        )
                    };
                    return Outcome::Required {
                        message,
                        path: path.clone(),
                    };
                }
            }
        }

        // `_process_extras()`: top-down, first non-empty `missing_` wins.
        for frame in &self.frames {
            let extras: Vec<&String> = frame
                .parsed
                .remaining
                .iter()
                .filter(|t| !t.starts_with('\0'))
                .collect();
            if !extras.is_empty() && !frame.app.allow_extras && !frame.app.prefix_command {
                // CLI11's `ExtrasError` (CLI11.hpp) builds its message with
                // `detail::rjoin`, which joins in REVERSE order — not the order
                // the extras were encountered on the command line.
                let message = if extras.len() > 1 {
                    format!(
                        "The following arguments were not expected: {}",
                        extras
                            .iter()
                            .rev()
                            .map(|s| s.as_str())
                            .collect::<Vec<_>>()
                            .join(" ")
                    )
                } else {
                    format!("The following argument was not expected: {}", extras[0])
                };
                return Outcome::Extras { message, path };
            }
        }

        // Everything checked out: run the matched callbacks bottom-up
        // (deepest first), the way `App::run_callback()` unwinds.
        let global = crate::app::global_options_from(&self.frames[0].parsed);
        let mut code = 0;
        for frame in self.frames.iter().rev() {
            if let Some(callback) = &frame.app.callback {
                code = callback(&frame.parsed, &global);
            }
        }
        Outcome::Ran { code, path }
    }
}

fn primary_name(opt: &Opt) -> &str {
    if !opt.positional.is_empty() {
        return &opt.positional;
    }
    opt.names
        .iter()
        .find(|n| n.starts_with("--"))
        .or_else(|| opt.names.first())
        .map(String::as_str)
        .unwrap_or("")
}

fn base_type_name(value_type: ValueType, override_name: Option<&str>) -> String {
    if let Some(name) = override_name {
        return name.to_string();
    }
    match value_type {
        ValueType::Text => "TEXT",
        ValueType::Int | ValueType::Int64 => "INT",
        ValueType::UInt | ValueType::UInt64 => "UINT",
        ValueType::Float | ValueType::Double => "FLOAT",
    }
    .to_string()
}

/// The `description()` CLI11 attaches to one `->check(...)` validator — what
/// `Option::get_type_name()` (CLI11.hpp) appends after `:` for each validator
/// on the option. `Range`/`RangeF` auto-generate `"<TYPE> in [min - max]"`
/// when constructed without an explicit name (CLI11.hpp's `Range` ctor);
/// `ExistingFile`/`PositiveNumber`/`NonNegativeNumber` are always given an
/// explicit literal name at construction, so their description is just that
/// name — note this is independent of the runtime failure text in
/// `check_validator`, which for `Range`-family validators always prints the
/// numeric bounds regardless of the name.
fn validator_description(validator: &Validator, value_type: ValueType) -> String {
    match validator {
        Validator::ExistingFile => "FILE".to_string(),
        Validator::Range(min, max) => {
            format!("{} in [{min} - {max}]", base_type_name(value_type, None))
        }
        Validator::RangeF(min, max) => {
            format!("{} in [{min} - {max}]", base_type_name(value_type, None))
        }
        Validator::PositiveNumber => "POSITIVE".to_string(),
        Validator::NonNegativeNumber => "NONNEGATIVE".to_string(),
        Validator::IsMember(choices) => format!("{{{}}}", choices.join(",")),
    }
}

/// `Option::get_type_name()` (CLI11.hpp): the base type name plus `:<description>`
/// for every attached validator, concatenated in order — this is what CLI11
/// embeds in `ArgumentMismatch::TypedAtLeast`'s "N required TYPE missing" message
/// (the help formatter keeps the simplified base name; only this error path uses
/// the full concatenation).
fn full_type_name(
    value_type: ValueType,
    override_name: Option<&str>,
    validators: &[Validator],
) -> String {
    let mut full = base_type_name(value_type, override_name);
    for validator in validators {
        full.push(':');
        full.push_str(&validator_description(validator, value_type));
    }
    full
}

/// One pass of C's `strtoull`/`strtoll(str, &end, 0)`, read at i128
/// precision: skips leading ASCII whitespace, an optional `+`/`-` sign,
/// then detects the base from a `0x`/`0X` prefix followed by at least one
/// hex digit (hex), else a leading `0` (octal — a lone `"0"` is base 8 with
/// zero further digits, value 0), else decimal, and consumes as many valid
/// digits of that base as it can. Returns the signed value only when the
/// consumed span reaches the exact end of `raw` (mirroring CLI11's own
/// `val == input.c_str() + input.size()` full-match check — a partial
/// parse, including a bare sign or an unconsumed `0x` with no hex digits
/// after it, is a failure here exactly as it is in the C++, which then
/// tries its own further fallbacks rather than accepting a partial parse).
fn strtoll_base0(raw: &str) -> Option<i128> {
    let bytes = raw.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() && (bytes[i] as char).is_ascii_whitespace() {
        i += 1;
    }
    let negative = match bytes.get(i) {
        Some(b'-') => {
            i += 1;
            true
        }
        Some(b'+') => {
            i += 1;
            false
        }
        _ => false,
    };
    let hex_prefix = bytes[i..].starts_with(b"0x") || bytes[i..].starts_with(b"0X");
    let (radix, digits_from): (u32, usize) = if hex_prefix
        && bytes
            .get(i + 2)
            .is_some_and(|b| (*b as char).is_ascii_hexdigit())
    {
        (16, i + 2)
    } else if bytes.get(i) == Some(&b'0') {
        (8, i)
    } else {
        (10, i)
    };
    let mut j = digits_from;
    let mut magnitude: i128 = 0;
    while j < bytes.len() {
        match (bytes[j] as char).to_digit(radix) {
            Some(d) => {
                magnitude = magnitude
                    .checked_mul(i128::from(radix))?
                    .checked_add(i128::from(d))?;
                j += 1;
            }
            None => break,
        }
    }
    if j == digits_from || j != bytes.len() {
        // No digit consumed at all (bare sign, "0x" with no hex digit
        // after it, all-whitespace input) or trailing garbage left over.
        return None;
    }
    Some(if negative { -magnitude } else { magnitude })
}

/// CLI11's `detail::integral_conversion<T>`: the base-0 numeral grammar
/// above, plus CLI11's own further fallbacks, tried in the same order the
/// C++ tries them: strip `_`/`'` digit separators and retry; if the string
/// ends in whitespace, trim both ends and retry; a `0o`/`0O` prefix
/// (octal, which a libc `strtoull` does not itself recognize); a `0b`/`0B`
/// prefix (binary). `unsigned_only` mirrors the *unsigned* overload's
/// upfront `input.front() == '-'` rejection — it never even calls
/// `strtoull` on a negative-looking string, unlike the signed overload,
/// which parses the sign and lets a negative result fail the caller's own
/// range check. Returns the exact mathematical value at i128 precision (far
/// wider than any C++ integer width wally binds an option to), so the
/// caller range-checks it against that option's own width — the same
/// effect as CLI11's per-`T` `static_cast` round-trip check, without a
/// generic parse per width.
fn cli_lexical_int(raw: &str, unsigned_only: bool) -> Option<i128> {
    if unsigned_only && raw.starts_with('-') {
        return None;
    }
    if let Some(v) = strtoll_base0(raw) {
        return Some(v);
    }
    if raw.contains(['_', '\'']) {
        let stripped: String = raw.chars().filter(|c| *c != '_' && *c != '\'').collect();
        if let Some(v) = cli_lexical_int(&stripped, unsigned_only) {
            return Some(v);
        }
    }
    if raw.ends_with(|c: char| c.is_ascii_whitespace()) {
        if let Some(v) = cli_lexical_int(raw.trim(), unsigned_only) {
            return Some(v);
        }
    }
    for (p1, p2, radix) in [("0o", "0O", 8u32), ("0b", "0B", 2u32)] {
        if let Some(digits) = raw.strip_prefix(p1).or_else(|| raw.strip_prefix(p2)) {
            if !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix)) {
                if let Ok(v) = i128::from_str_radix(digits, radix) {
                    return Some(v);
                }
            }
        }
    }
    None
}

/// CLI11's `detail::to_flag_value` + boolean `lexical_cast`, for converting a
/// `--flag=value` inline value on a boolean flag option. Mirrors: exact
/// case-sensitive `"true"`/`"false"`, then lowercase, then a single-char
/// special case, then a multi-char keyword set, then a numeric fallback via
/// `strtoll` where the sign of the result decides the boolean (`out > 0`).
/// Returns `None` when CLI11 would leave `errno == EINVAL` (conversion
/// fails, i.e. "Could not convert").
fn flag_value_bool(input: &str) -> Option<bool> {
    if input == "true" {
        return Some(true);
    }
    if input == "false" {
        return Some(false);
    }
    let lower = input.to_ascii_lowercase();
    if lower.chars().count() == 1 {
        return match lower.chars().next().unwrap() {
            '1'..='9' => Some(true),
            '0' | 'f' | 'n' | '-' => Some(false),
            't' | 'y' | '+' => Some(true),
            _ => None,
        };
    }
    match lower.as_str() {
        "true" | "on" | "yes" | "enable" => return Some(true),
        "false" | "off" | "no" | "disable" => return Some(false),
        _ => {}
    }
    strtoll_base0(&lower).map(|v| v > 0)
}

/// The inclusive `[min, max]` a specific C++ integer width can hold, at
/// i128 precision — what `cli_lexical_int`'s result gets range-checked
/// against, standing in for CLI11's per-`T` round-trip cast check. A handful
/// of options are bound to a C++ width narrower than any `ValueType` variant
/// distinguishes (e.g. `serve --port`'s `uint16_t`); those attach an
/// `Opt::int_bound_override` instead of a dedicated `ValueType` variant, so
/// this table only needs one arm per *distinct* `ValueType`, matching
/// `cli_formatter.rs`'s own (separately owned) exhaustive match on the same
/// enum.
fn integer_bounds(value_type: ValueType) -> (i128, i128) {
    match value_type {
        ValueType::Int => (i128::from(i32::MIN), i128::from(i32::MAX)),
        ValueType::Int64 => (i128::from(i64::MIN), i128::from(i64::MAX)),
        ValueType::UInt => (0, i128::from(u32::MAX)),
        ValueType::UInt64 => (0, i128::from(u64::MAX)),
        ValueType::Text | ValueType::Float | ValueType::Double => {
            (i128::from(i64::MIN), i128::from(i64::MAX))
        }
    }
}

/// Validates `raw` against `validators` and its bound type, and returns the
/// canonical string to store (identical to `raw` for everything except: an
/// empty value, which CLI11's `detail::lexical_assign` converts to the
/// type's default-constructed zero/empty value *without* ever running the
/// type's own `lexical_cast` — skipping hex/octal parsing and range checks
/// entirely — regardless of what `raw` would otherwise mean; and a numeral
/// written in a form `lexical_cast` accepts but a plain decimal parse
/// wouldn't, e.g. `0x10` or `' 5'`, which are canonicalized to plain
/// decimal here so every later reader (`get_i64`, `get_str`, …) sees the
/// same value CLI11's callback would have seen). Validators still run
/// first, on the raw text, even when it's empty — CLI11's own
/// `Option::_validate` only special-cases an empty result when the option
/// itself expects zero values, which is never true for a normal wally
/// option (confirmed against the real C++ binary: `telemetry emit --count
/// ''` still hits the `PositiveNumber` validator's failure text, not a
/// silent zero).
fn validate_and_convert(
    display: &str,
    raw: &str,
    value_type: ValueType,
    validators: &[Validator],
    int_bound_override: Option<(i128, i128)>,
) -> Result<String, String> {
    for validator in validators {
        if let Some(msg) = check_validator(validator, raw) {
            return Err(format!("{display}: {msg}"));
        }
    }
    if raw.is_empty() {
        return Ok(match value_type {
            ValueType::Text => String::new(),
            _ => "0".to_string(),
        });
    }
    match value_type {
        ValueType::Text => Ok(raw.to_string()),
        ValueType::Int | ValueType::Int64 | ValueType::UInt | ValueType::UInt64 => {
            let unsigned = matches!(value_type, ValueType::UInt | ValueType::UInt64);
            let (min, max) = int_bound_override.unwrap_or_else(|| integer_bounds(value_type));
            match cli_lexical_int(raw, unsigned) {
                Some(v) if v >= min && v <= max => Ok(v.to_string()),
                _ => Err(format!("Could not convert: {display} = {raw}")),
            }
        }
        ValueType::Float | ValueType::Double => match raw.trim().parse::<f64>() {
            Ok(_) => Ok(raw.to_string()),
            Err(_) => Err(format!("Could not convert: {display} = {raw}")),
        },
    }
}

/// One `->check(...)` validator, run against the raw string (CLI11 validates
/// before the option's own type conversion). Returns `None` on success.
fn check_validator(validator: &Validator, raw: &str) -> Option<String> {
    match validator {
        Validator::ExistingFile => {
            if std::path::Path::new(raw).is_file() {
                None
            } else {
                Some(format!("File does not exist: {raw}"))
            }
        }
        Validator::Range(min, max) => match raw.trim().parse::<i64>() {
            Ok(v) if v >= *min && v <= *max => None,
            _ => Some(format!("Value {raw} not in range [{min} - {max}]")),
        },
        Validator::RangeF(min, max) => match raw.trim().parse::<f64>() {
            Ok(v) if v >= *min && v <= *max => None,
            _ => Some(format!("Value {raw} not in range [{min} - {max}]")),
        },
        // `PositiveNumber`/`NonNegativeNumber` are CLI11's `Range(DBL_MIN, DBL_MAX,
        // "POSITIVE")` / `Range(0, DBL_MAX, "NONNEGATIVE")`: the validator's *name*
        // ("POSITIVE"/"NONNEGATIVE") only ever surfaces in `description()` (used for
        // the type name, see `validator_description` below) — the runtime failure
        // string always prints the literal numeric bounds captured at construction
        // (CLI11.hpp's `Range::func_`), never the name.
        Validator::PositiveNumber => match raw.trim().parse::<f64>() {
            Ok(v) if v > 0.0 => None,
            _ => Some(format!(
                "Value {raw} not in range [2.22507e-308 - 1.79769e+308]"
            )),
        },
        Validator::NonNegativeNumber => match raw.trim().parse::<f64>() {
            Ok(v) if v >= 0.0 => None,
            _ => Some(format!("Value {raw} not in range [0 - 1.79769e+308]")),
        },
        Validator::IsMember(choices) => {
            if choices.iter().any(|c| c == raw) {
                None
            } else {
                Some(format!("{raw} not in {{{}}}", choices.join(",")))
            }
        }
    }
}

/// Registers every name (and the positional name) of every option on `app`
/// into `parsed.name_to_spec`/`defaults`, so `Parsed::get_str` etc. resolve
/// by any of an option's names, the way CLI11's `Option` does internally.
fn index_names(app: &App, parsed: &mut Parsed) {
    for opt in &app.options {
        if !opt.positional.is_empty() {
            parsed
                .name_to_spec
                .insert(opt.positional.clone(), opt.spec.clone());
        }
        for name in &opt.names {
            parsed.name_to_spec.insert(name.clone(), opt.spec.clone());
        }
        if let Some(default) = &opt.default_value {
            parsed.defaults.insert(opt.spec.clone(), default.clone());
        }
    }
}

/// What one (sub)command received on the command line, after conversion and
/// validation. Look values up by any of the option's names or the positional
/// name. Unset options report their `default_val`, if any.
#[derive(Clone, Debug, Default)]
pub struct Parsed {
    /// Primary name of the command this belongs to.
    pub name: String,
    /// Raw string values per option, keyed by the option's `spec`.
    pub values: BTreeMap<String, Vec<String>>,
    /// Defaults per option spec (from `default_val`).
    pub defaults: BTreeMap<String, String>,
    /// Name → spec, for every name and positional name of every option.
    pub name_to_spec: BTreeMap<String, String>,
    /// Flag results per option spec (after `{value}` suffixes).
    pub flag_values: BTreeMap<String, Vec<String>>,
    /// Leftover arguments (prefix_command / allow_extras), in order.
    pub remaining: Vec<String>,
}

impl Parsed {
    fn spec(&self, name: &str) -> Option<&String> {
        self.name_to_spec.get(name)
    }

    /// How many times the option/flag/positional was given.
    pub fn count(&self, name: &str) -> usize {
        self.spec(name)
            .map(|s| {
                self.values.get(s).map_or(0, Vec::len) + self.flag_values.get(s).map_or(0, Vec::len)
            })
            .unwrap_or(0)
    }

    /// Given on the command line at all (CLI11 `opt->count() > 0`).
    pub fn is_set(&self, name: &str) -> bool {
        self.count(name) > 0
    }

    /// A flag's boolean result. Honors `{false}` names: the last occurrence wins,
    /// as when CLI11 assigns a bool flag.
    pub fn flag(&self, name: &str) -> bool {
        let Some(spec) = self.spec(name) else {
            return false;
        };
        match self.flag_values.get(spec).and_then(|v| v.last()) {
            Some(value) => !matches!(value.as_str(), "false" | "0" | "off" | "no"),
            None => self
                .defaults
                .get(spec)
                .is_some_and(|d| d == "true" || d == "1"),
        }
    }

    /// The last value given, else the default.
    pub fn get_str(&self, name: &str) -> Option<String> {
        let spec = self.spec(name)?;
        self.values
            .get(spec)
            .and_then(|v| v.last().cloned())
            .or_else(|| self.defaults.get(spec).cloned())
    }

    /// Every value given (vector options and positionals), else the default as one value.
    pub fn get_strs(&self, name: &str) -> Vec<String> {
        let Some(spec) = self.spec(name) else {
            return Vec::new();
        };
        match self.values.get(spec) {
            Some(v) if !v.is_empty() => v.clone(),
            _ => self
                .defaults
                .get(spec)
                .map(|d| vec![d.clone()])
                .unwrap_or_default(),
        }
    }

    pub fn get_i64(&self, name: &str) -> Option<i64> {
        self.get_str(name).and_then(|v| v.trim().parse().ok())
    }

    pub fn get_u64(&self, name: &str) -> Option<u64> {
        self.get_str(name).and_then(|v| v.trim().parse().ok())
    }

    pub fn get_f64(&self, name: &str) -> Option<f64> {
        self.get_str(name).and_then(|v| v.trim().parse().ok())
    }

    /// Leftover arguments for prefix commands (the wrapped tool's argv).
    pub fn remaining(&self) -> &[String] {
        &self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A root flag and option are readable in the root's own callback, by
    /// any of their registered names.
    #[test]
    fn root_flag_and_option_are_readable_in_callback() {
        let captured = Rc::new(RefCell::new(None));
        let captured2 = captured.clone();
        let mut app = App::new("desc", "wally");
        app.add_flag("--json", "json flag");
        app.add_option("--home", ValueType::Text, "home dir");
        app.callback(move |p, _g| {
            *captured2.borrow_mut() = Some((p.flag("--json"), p.get_str("--home")));
            0
        });

        let outcome = app.parse(&["--json".into(), "--home".into(), "/tmp/x".into()]);

        assert_eq!(
            outcome,
            Outcome::Ran {
                code: 0,
                path: vec![]
            }
        );
        assert_eq!(*captured.borrow(), Some((true, Some("/tmp/x".to_string()))));
    }

    /// A subcommand's required positional is filled and readable, and the
    /// deepest command's callback result becomes the process exit code.
    #[test]
    fn subcommand_positional_and_required_option() {
        let mut app = App::new("root", "wally");
        app.add_subcommand("pull", "Download a model")
            .add_option("model", ValueType::Text, "model id")
            .required();
        app.get_subcommand_mut("pull").unwrap().callback(|p, _g| {
            assert_eq!(p.get_str("model").as_deref(), Some("qwen3-0.6b"));
            7
        });

        let outcome = app.parse(&["pull".into(), "qwen3-0.6b".into()]);

        assert_eq!(
            outcome,
            Outcome::Ran {
                code: 7,
                path: vec!["pull".to_string()]
            }
        );
    }

    /// CLI11's RequiredError: a required option left off the command line,
    /// with the exact "<name> is required" message.
    #[test]
    fn missing_required_option_is_required_error() {
        let mut app = App::new("root", "wally");
        app.add_subcommand("pull", "Download a model")
            .add_option("model", ValueType::Text, "model id")
            .required();

        let outcome = app.parse(&["pull".into()]);

        assert_eq!(
            outcome,
            Outcome::Required {
                message: "model is required".to_string(),
                path: vec!["pull".to_string()],
            }
        );
    }

    /// CLI11's ExtrasError (singular form): an argument nothing was
    /// registered to absorb.
    #[test]
    fn unexpected_positional_is_extras_error() {
        let mut app = App::new("root", "wally");
        app.add_subcommand("about", "About wally");

        let outcome = app.parse(&["about".into(), "extra".into()]);

        assert_eq!(
            outcome,
            Outcome::Extras {
                message: "The following argument was not expected: extra".to_string(),
                path: vec!["about".to_string()],
            }
        );
    }

    /// A `->check(CLI::Range(...))` validator failure is a ParseError with no
    /// help block, using CLI11's own "Value X not in range [min - max]" text.
    #[test]
    fn range_validator_produces_parse_error() {
        let mut app = App::new("root", "wally");
        app.add_option("--top-k", ValueType::Int, "sampling top-k")
            .check(Validator::Range(1, 100));

        let outcome = app.parse(&["--top-k".into(), "500".into()]);

        assert_eq!(
            outcome,
            Outcome::ParseErr {
                message: "--top-k: Value 500 not in range [1 - 100]".to_string(),
            }
        );
    }

    /// CLI11's `ExtrasError` (`detail::rjoin`) lists multiple unexpected
    /// arguments in REVERSE order, not the order they were typed.
    #[test]
    fn extras_error_lists_multiple_unexpected_arguments_in_reverse_order() {
        let mut app = App::new("root", "wally");
        app.add_subcommand("about", "About wally");

        let outcome = app.parse(&[
            "about".into(),
            "one".into(),
            "two".into(),
            "three".into(),
            "four".into(),
        ]);

        assert_eq!(
            outcome,
            Outcome::Extras {
                message: "The following arguments were not expected: four three two one"
                    .to_string(),
                path: vec!["about".to_string()],
            }
        );
    }

    /// A `std::vector<std::string>`-bound (`multi`) option still has
    /// `type_size(1, 1)`: one occurrence consumes exactly one token, so a
    /// trailing bare `--stop` is a missing-value ParseError, not a silently
    /// empty vector — repetition, not greedy per-occurrence consumption, is
    /// how CLI11 fills a vector option.
    #[test]
    fn multi_option_with_no_trailing_value_is_missing_value_error() {
        let mut app = App::new("root", "wally");
        app.add_option("--stop", ValueType::Text, "stop sequence")
            .multi();

        let outcome = app.parse(&["--stop".into()]);

        assert_eq!(
            outcome,
            Outcome::ParseErr {
                message: "--stop: 1 required TEXT missing".to_string(),
            }
        );
    }

    /// The "N required TYPE missing" message uses CLI11's full
    /// `Option::get_type_name()` (base type plus `:<validator description>`),
    /// not just the base type name the help formatter simplifies to.
    #[test]
    fn missing_value_error_includes_validator_text_in_type_name() {
        let mut app = App::new("root", "wally");
        app.add_option("--max-output-tokens", ValueType::Int, "cap")
            .check(Validator::Range(1, 2147483647));

        let outcome = app.parse(&["--max-output-tokens".into()]);

        assert_eq!(
            outcome,
            Outcome::ParseErr {
                message: "--max-output-tokens: 1 required INT:INT in [1 - 2147483647] missing"
                    .to_string(),
            }
        );
    }

    /// `PositiveNumber`'s failure text is CLI11's `Range(double)` text using
    /// the literal DBL_MIN/DBL_MAX bounds captured at construction — not the
    /// validator's "POSITIVE" name (which only appears in the type name).
    #[test]
    fn positive_number_validator_uses_literal_double_bounds_in_failure_text() {
        let mut app = App::new("root", "wally");
        app.add_option("--count", ValueType::Int, "count")
            .check(Validator::PositiveNumber);

        let outcome = app.parse(&["--count".into(), "0".into()]);

        assert_eq!(
            outcome,
            Outcome::ParseErr {
                message: "--count: Value 0 not in range [2.22507e-308 - 1.79769e+308]".to_string(),
            }
        );
    }

    /// The passthrough contract: a `prefix_command` subcommand's own `-m`
    /// option is consumed first, and every remaining plain token greedily
    /// fills the `multi` "args" positional — exactly the tool argv a wrapped
    /// coding tool receives (`wally opencode -m x run hello` -> `run hello`).
    #[test]
    fn passthrough_style_multi_positional_takes_rest_of_line() {
        let captured = Rc::new(RefCell::new(Vec::new()));
        let captured2 = captured.clone();
        let mut app = App::new("root", "wally");
        {
            let sub = app.add_subcommand("opencode", "Open Code");
            sub.prefix_command(true);
            sub.add_option("-m,--model", ValueType::Text, "model");
            sub.add_option("args", ValueType::Text, "tool argv").multi();
            sub.callback(move |p, _g| {
                *captured2.borrow_mut() = p.get_strs("args");
                0
            });
        }

        let outcome = app.parse(&[
            "opencode".into(),
            "-m".into(),
            "x".into(),
            "run".into(),
            "hello".into(),
        ]);

        assert_eq!(
            outcome,
            Outcome::Ran {
                code: 0,
                path: vec!["opencode".to_string()]
            }
        );
        assert_eq!(
            *captured.borrow(),
            vec!["run".to_string(), "hello".to_string()]
        );
    }

    /// `-h`/`--help` does not short-circuit the scan: the parser keeps
    /// descending into later subcommands, so `wally --help models pull`
    /// reports the deepest command reached, not the root.
    #[test]
    fn help_flag_anywhere_matches_deepest_reached_command() {
        let mut app = App::new("root", "wally");
        app.set_help_flag("-h,--help", "Show help");
        {
            let models = app.add_subcommand("models", "Manage models");
            models.add_subcommand("pull", "Download a model");
        }

        let outcome = app.parse(&["--help".into(), "models".into(), "pull".into()]);

        assert_eq!(
            outcome,
            Outcome::Help {
                path: vec!["models".to_string(), "pull".to_string()]
            }
        );
    }

    /// `-V`/`--version` reports the exact text `set_version_flag` was given.
    #[test]
    fn version_flag_returns_configured_text() {
        let mut app = App::new("root", "wally");
        app.set_version_flag("--version,-V", "wally 1.2.3", "Show version");

        let outcome = app.parse(&["--version".into()]);

        assert_eq!(
            outcome,
            Outcome::Version {
                text: "wally 1.2.3".to_string()
            }
        );
    }
}
