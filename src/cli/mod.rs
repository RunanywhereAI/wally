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
    pub fn add_subcommand(&mut self, name: &str, description: &str) -> &mut App {
        let _ = (name, description);
        todo!("CLI port: construct with CLI11's parent inheritance, push, return it")
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
        let _ = (spec, value_type, description);
        todo!("CLI port: split the CLI11 name spec into names/positional, push, return it")
    }

    /// `add_flag(spec, variable, description)`, including CLI11's
    /// `--on,--off{false}` value suffixes.
    pub fn add_flag(&mut self, spec: &str, description: &str) -> &mut Opt {
        let _ = (spec, description);
        todo!("CLI port: split the CLI11 flag spec (with {{value}} suffixes), push, return it")
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
