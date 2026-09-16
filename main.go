// Command wally is the RunAnywhere product CLI.
//
// This is the start of the Go rewrite: the CLI glue (commands, harness
// launchers, the console device-flow login, the OpenAI/Anthropic shim) is
// Go, while inference stays in the existing C++ desktop kit and the Swift MLX
// host, reached over the local rac_server HTTP endpoint or through cgo.
package main

import (
	"fmt"
	"os"
)

// version is the product version, independent of the SDK kit.
const version = "0.6.0-dev"

func main() {
	os.Exit(run(os.Args[1:]))
}

func run(args []string) int {
	if len(args) == 0 {
		usage()
		return 0
	}
	switch args[0] {
	case "version", "--version", "-V":
		fmt.Printf("wally %s\n", version)
		return 0
	case "help", "--help", "-h":
		usage()
		return 0
	default:
		fmt.Fprintf(os.Stderr, "wally: unknown command %q\n", args[0])
		usage()
		return 2
	}
}

func usage() {
	fmt.Println("wally — RunAnywhere on-device AI CLI")
	fmt.Println()
	fmt.Println("usage: wally <command> [options]")
	fmt.Println()
	fmt.Println("commands:")
	fmt.Println("  version   print the wally version")
	fmt.Println("  help      show this help")
}
