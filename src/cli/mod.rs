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
    /// unsigned / uint32_t (also uint16_t ports)
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
    /// succeeded); carries its exit code.
    Ran(i32),
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
    Long { name: &'a str, inline: Option<&'a str> },
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
        self.frames[1..].iter().map(|f| f.app.name.clone()).collect()
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
                        i = self.consume_short(args, i, name, rest);
                        continue;
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
    fn consume_option(&mut self, args: &[String], i: usize, name: &str, inline: Option<&str>) -> usize {
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
            if let Some(opt_idx) = self.frames[level].app.options.iter().position(|o| o.names.iter().any(|n| n == name)) {
                return self.consume_matched(args, i, level, opt_idx, inline, None);
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
                self.version_text = Some(self.frames[level].app.version_flag.as_ref().unwrap().1.clone());
                return i + 1;
            }
            if let Some(opt_idx) = self.frames[level].app.options.iter().position(|o| o.names.iter().any(|n| n == name)) {
                let inline = if rest.is_empty() { None } else { Some(rest) };
                return self.consume_matched(args, i, level, opt_idx, inline, Some(rest));
            }
        }
        self.handle_unmatched(depth, args[i].clone(), i)
    }

    /// Records one match of `app.options[opt_idx]` at frame `level`: a flag
    /// (records its `{value}` result) or an option (consumes one value, or —
    /// for a `multi` option — every following plain token, CLI11's
    /// `TakeAll`/vector-positional-style greedy consumption).
    fn consume_matched(
        &mut self,
        args: &[String],
        i: usize,
        level: usize,
        opt_idx: usize,
        inline: Option<&str>,
        short_cluster_rest: Option<&str>,
    ) -> usize {
        let is_flag = self.frames[level].app.options[opt_idx].is_flag;
        if is_flag {
            let matched_name = args[i]
                .split('=')
                .next()
                .unwrap_or(&args[i])
                .split(|c: char| c != '-' && !c.is_ascii_alphanumeric())
                .next()
                .unwrap_or(&args[i]);
            let _ = matched_name;
            // Flags never take a value; look up which registered name was
            // typed to resolve its `{value}` result (defaults to "true").
            let opt = &self.frames[level].app.options[opt_idx];
            let typed_name = opt
                .names
                .iter()
                .find(|n| args[i].starts_with(n.as_str()))
                .cloned()
                .unwrap_or_default();
            let value = opt.flag_values.get(&typed_name).cloned().unwrap_or_else(|| "true".to_string());
            let spec = opt.spec.clone();
            self.frames[level].parsed.flag_values.entry(spec).or_default().push(value);
            return if short_cluster_rest.is_some_and(|r| !r.is_empty()) && inline.is_none() {
                i + 1
            } else {
                i + 1
            };
        }

        let multi = self.frames[level].app.options[opt_idx].multi;
        let spec = self.frames[level].app.options[opt_idx].spec.clone();
        let value_type = self.frames[level].app.options[opt_idx].value_type;
        let validators = self.frames[level].app.options[opt_idx].validators.clone();
        let type_name = self.frames[level].app.options[opt_idx].type_name.clone();
        let display = self.frames[level].app.options[opt_idx]
            .names
            .iter()
            .find(|n| n.starts_with("--"))
            .or_else(|| self.frames[level].app.options[opt_idx].names.first())
            .cloned()
            .unwrap_or_default();

        let mut next = i + 1;
        let mut values: Vec<String> = Vec::new();
        if let Some(v) = inline {
            values.push(v.to_string());
        } else if !multi {
            match args.get(next) {
                Some(v) if !looks_like_flag(v) || multi => {
                    values.push(v.clone());
                    next += 1;
                }
                _ => {
                    self.push_parse_error(format!(
                        "{display}: 1 required {} missing",
                        base_type_name(value_type, type_name.as_deref())
                    ));
                    return next;
                }
            }
        } else {
            while let Some(v) = args.get(next) {
                if looks_like_flag(v) {
                    break;
                }
                values.push(v.clone());
                next += 1;
            }
        }

        for raw in &values {
            if let Err(msg) = validate_and_convert(&display, raw, value_type, &validators) {
                self.push_parse_error(msg);
                return next;
            }
        }
        self.frames[level].parsed.values.entry(spec).or_default().extend(values);
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
            frame.parsed.values.entry(spec).or_default().push(token.to_string());
            if !multi {
                frame.positional_cursor += 1;
            }
        } else if frame.app.prefix_command || frame.app.allow_extras {
            frame.parsed.remaining.push(token.to_string());
        } else {
            frame.parsed.remaining.push(token.to_string());
        }
    }

    fn push_parse_error(&mut self, message: String) {
        self.frames.last_mut().unwrap().parsed.remaining.push(format!("\0parse-error\0{message}"));
    }

    fn finish(self) -> Outcome {
        // A bad conversion/argument count is recorded as a sentinel in
        // `remaining` the moment it happens (CLI11 throws immediately, from
        // wherever the bad token was) — surface the first one, in scan order,
        // ahead of anything else.
        for frame in &self.frames {
            if let Some(msg) = frame.parsed.remaining.iter().find_map(|r| r.strip_prefix("\0parse-error\0")) {
                return Outcome::ParseErr { message: msg.to_string() };
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
                    return Outcome::Required { message, path: path.clone() };
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
                let message = if extras.len() > 1 {
                    format!(
                        "The following arguments were not expected: {}",
                        extras.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(" ")
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
        Outcome::Ran(code)
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

/// Whether CLI11 would classify `token` as option-shaped (so a `multi`
/// option/positional stops greedily consuming at it). A `-`-prefixed
/// negative number is deliberately still "looks like a flag" here — none of
/// wally's vector options are ever fed literal negative numbers, and CLI11's
/// own carve-out for them (checking whether a same-named short option exists)
/// is not worth the complexity it would add throughout the resolver.
fn looks_like_flag(token: &str) -> bool {
    token.starts_with('-') && token != "-"
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

/// CLI11's base-0 integer `lexical_cast`: `0x`/`0X` hex, a lone leading `0`
/// with more digits octal, otherwise decimal; `_`/`'` are digit separators.
fn parse_cli_int(raw: &str) -> Option<i64> {
    let cleaned: String = raw.chars().filter(|c| *c != '_' && *c != '\'').collect();
    let (neg, digits) = match cleaned.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, cleaned.strip_prefix('+').unwrap_or(&cleaned)),
    };
    let value = if let Some(hex) = digits.strip_prefix("0x").or_else(|| digits.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()?
    } else if digits.len() > 1 && digits.starts_with('0') && digits.chars().all(|c| c.is_digit(8)) {
        i64::from_str_radix(digits, 8).ok()?
    } else {
        digits.parse::<i64>().ok()?
    };
    Some(if neg { -value } else { value })
}

fn validate_and_convert(
    display: &str,
    raw: &str,
    value_type: ValueType,
    validators: &[Validator],
) -> Result<(), String> {
    for validator in validators {
        if let Some(msg) = check_validator(validator, raw) {
            return Err(format!("{display}: {msg}"));
        }
    }
    let ok = match value_type {
        ValueType::Text => true,
        ValueType::Int | ValueType::Int64 => parse_cli_int(raw).is_some(),
        ValueType::UInt | ValueType::UInt64 => parse_cli_int(raw).is_some_and(|v| v >= 0),
        ValueType::Float | ValueType::Double => raw.trim().parse::<f64>().is_ok(),
    };
    if !ok {
        return Err(format!("Could not convert: {display} = {raw}"));
    }
    Ok(())
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
        Validator::PositiveNumber => match raw.trim().parse::<f64>() {
            Ok(v) if v > 0.0 => None,
            _ => Some(format!("Value {raw} not in range [POSITIVE]")),
        },
        Validator::NonNegativeNumber => match raw.trim().parse::<f64>() {
            Ok(v) if v >= 0.0 => None,
            _ => Some(format!("Value {raw} not in range [NONNEGATIVE]")),
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
            parsed.name_to_spec.insert(opt.positional.clone(), opt.spec.clone());
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
