//go:build windows

package cmd

import "os/exec"

func detach(c *exec.Cmd) {}
