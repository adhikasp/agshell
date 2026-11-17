// agshell — a supervised execution shell for coding agents.
//
// It parses a bash script, then runs it one element at a time so that the
// outcome of every command and every pipeline stage survives as data. bash
// still evaluates the words; agshell owns the topology.
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"os"
	"strings"
	"time"
)

const version = "0.1.0"

func main() {
	var (
		script  string
		timeout time.Duration
		human   bool
		maxOut  int
		showVer bool
	)
	fs := flag.NewFlagSet("agshell", flag.ExitOnError)
	fs.StringVar(&script, "c", "", "bash script to supervise")
	fs.DurationVar(&timeout, "timeout", 30*time.Second, "per-command timeout (0 disables)")
	fs.BoolVar(&human, "human", false, "print a report instead of the JSON envelope")
	fs.IntVar(&maxOut, "max-stdout", 65536, "captured stdout limit in bytes")
	fs.BoolVar(&showVer, "version", false, "print version")

	args := os.Args[1:]
	if len(args) > 0 && args[0] == "run" {
		args = args[1:]
	}
	if err := fs.Parse(args); err != nil {
		os.Exit(2)
	}
	if showVer {
		fmt.Println("agshell", version)
		return
	}
	if script == "" {
		if rest := fs.Args(); len(rest) > 0 {
			script = strings.Join(rest, " ")
		}
	}
	if script == "" {
		fmt.Fprintln(os.Stderr, "usage: agshell [-c] '<bash script>' [--timeout 30s] [--human]")
		os.Exit(2)
	}

	res := NewRunner(script, timeout, maxOut).Run()

	if human {
		printHuman(script, res)
	} else {
		enc := json.NewEncoder(os.Stdout)
		enc.SetEscapeHTML(false) // shell text should read like shell text
		enc.SetIndent("", "  ")
		_ = enc.Encode(res)
	}
	os.Exit(res.Exit)
}

func printHuman(script string, r Result) {
	fmt.Printf("$ %s\n\n", script)
	for _, c := range r.Commands {
		state := fmt.Sprintf("status=%d", c.Status)
		switch {
		case c.Skipped:
			state = "skipped"
		case c.TimedOut:
			state = "TIMEOUT"
		}
		tag := ""
		if c.PipelineID != 0 {
			tag = fmt.Sprintf(" pipe%d.%d", c.PipelineID, c.Stage)
		}
		if c.Delegated {
			tag += " delegated"
		}
		fmt.Printf("  #%d  %-34s %-9s %4dms%s\n", c.Index, trunc(c.Text, 34), state, c.Ms, tag)
		if c.Stderr != "" {
			fmt.Printf("       stderr: %s\n", trunc(strings.TrimSpace(c.Stderr), 100))
		}
	}
	fmt.Printf("\nclass=%s exit=%d %dms", r.Class, r.Exit, r.DurationMs)
	if len(r.HiddenFailures) > 0 {
		fmt.Printf(" hidden_failures=%v", r.HiddenFailures)
	}
	if r.StdoutTruncated {
		fmt.Printf(" stdout_truncated")
	}
	fmt.Println()
	if s := strings.TrimRight(r.Stdout, "\n"); s != "" {
		fmt.Printf("\n--- stdout ---\n%s\n", s)
	}
	if r.Error != "" {
		fmt.Printf("\n--- error ---\n%s\n", r.Error)
	}
}

func trunc(s string, n int) string {
	s = strings.ReplaceAll(s, "\n", " ")
	if len(s) > n {
		return s[:n] + "…"
	}
	return s
}
