# wally

The RunAnywhere product CLI, being rewritten in Go.

The CLI layer — commands, coding-agent launchers, the console device-flow
login, and the OpenAI/Anthropic translation — lives here in Go. Inference is
not rewritten: it stays in the existing C++ desktop kit and the Swift MLX host,
reached over the local `rac_server` HTTP endpoint or through cgo.

## Build

```sh
go build -o wally .
./wally version
```

## Install

`install.sh` (POSIX) and `install.ps1` (Windows) fetch a released binary. They
still point at the previous release layout and will be updated for the Go
build.
