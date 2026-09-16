# Harnesses

A harness is a coding tool wally knows how to point at a model: Claude Code,
Claude Desktop, OpenCode, Hermes, OpenClaw, DeepSeek Harness today. Each one is
a `wally <name>` subcommand, generated from a registry, not a hand-written
command per tool.

## What a harness is, in code

```go
type Wire func(ep runanywhere.Endpoint, model string, args []string) (env, argv []string, cleanup func(), err error)

type Harness struct {
    Name    string
    Command string
    Summary string
    Wire    Wire
    Impl    any
}
```

`Wire` is the only required piece: given a resolved model endpoint, it returns
the environment and argv to launch the tool with, and a cleanup func to run
when the tool exits. Everything else is an opt-in capability, detected by type
assertion against `Impl`:

| Capability | Question it answers |
|---|---|
| `Installable` | Is the tool installed, and if not, how do you install it? |
| `ModelLimits` | Does this tool need its context/output limits injected? |
| `ConfigPreserving` | Does launching it need to touch the tool's own config, and if so, copy/restore rather than overwrite? |
| `Restorable` | Does anything need undoing when the tool exits? |

A harness only implements the capabilities that apply to it; the launcher
checks each one through an accessor rather than assuming every harness has
every hook.

## What `wally <harness>` actually does

1. Ensure the daemon is up (start it if not, see [daemon.md](daemon.md)).
2. Check the tool is installed; if not, surface its install hint.
3. Resolve the model (`--model`, or the harness's default).
4. Call `Wire` to get env, argv, and cleanup.
5. Spawn the tool with that env and argv.
6. Run cleanup when it exits.

## Adding one

Add a `Harness` value to `harness.Registry` with a `Wire` func and whichever
capability interfaces apply. `wally <name>` appears automatically; nothing in
`cmd/` needs to change by hand.
