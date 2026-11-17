package main

import (
	"regexp"
	"strings"
	"time"
)

// Cmd is one supervised element of a command. A simple command produces one.
// Each stage of a pipeline produces one. A statement agshell did not look
// inside (subshell, for/if/case, !, &, a construct with redirections it cannot
// reproduce) produces one with Delegated set.
type Cmd struct {
	Index      int    `json:"i"`
	Text       string `json:"text"`
	Bin        string `json:"bin,omitempty"`
	Status     int    `json:"status"` // -1 means skipped by && or ||
	Skipped    bool   `json:"skipped,omitempty"`
	TimedOut   bool   `json:"timed_out,omitempty"`
	Ms         int64  `json:"ms"`
	Stderr     string `json:"stderr,omitempty"`
	PipelineID int    `json:"pipeline,omitempty"` // 0 = standalone
	Stage      int    `json:"stage,omitempty"`
	Delegated  bool   `json:"delegated,omitempty"`
}

// Result is the envelope. This is the point of agshell: a command's outcome
// as data, including the statuses the shell text channel destroys.
type Result struct {
	Class      string `json:"class"`
	Exit       int    `json:"exit"`
	DurationMs int64  `json:"duration_ms"`
	Commands   []Cmd  `json:"commands"`
	// HiddenFailures indexes commands that failed while the process exit code
	// still said 0. This is the whole argument in one field.
	HiddenFailures     []int  `json:"hidden_failures,omitempty"`
	Stdout             string `json:"stdout"`
	StdoutBytes        int    `json:"stdout_bytes"`
	StdoutTruncated    bool   `json:"stdout_truncated,omitempty"`
	OpaqueSubstitution bool   `json:"opaque_substitution,omitempty"`
	Error              string `json:"error,omitempty"`
}

var errLine = regexp.MustCompile(
	`(?mi)^.*(?:\berror\s*:|\bERROR\b|Traceback \(most recent call last\)|` +
		`\bpanic:|\bCannot (?:find|open|access|bind|start)|` +
		`\bcommand not found\b|\bNo such file\b|\bFAILED\b|\bnot permitted\b|` +
		`\bpermission denied\b|\bfatal:)`)

// classify turns the collected statuses into the one word a caller acts on.
// Order matters: a hidden pipeline failure outranks a hidden chain failure,
// and both outrank "it exited 0 and printed nothing wrong".
func classify(cmds []Cmd, exit int) string {
	timeout, stageFailed, anyFailed := false, false, false
	for _, c := range cmds {
		if c.Skipped {
			continue
		}
		if c.TimedOut {
			timeout = true
		}
		if c.Status != 0 {
			anyFailed = true
			if c.PipelineID != 0 && c.Stage < maxStage(cmds, c.PipelineID) {
				stageFailed = true
			}
		}
	}
	switch {
	case timeout:
		return "timeout"
	case exit != 0:
		return "failed"
	case stageFailed:
		return "stage_failed"
	case anyFailed:
		return "chain_partial"
	default:
		return "ok"
	}
}

func maxStage(cmds []Cmd, pipeline int) int {
	m := 0
	for _, c := range cmds {
		if c.PipelineID == pipeline && c.Stage > m {
			m = c.Stage
		}
	}
	return m
}

// hidden lists failed commands whose status the process exit code did not show.
func hidden(cmds []Cmd, exit int) []int {
	var out []int
	last := -1
	for _, c := range cmds {
		if !c.Skipped {
			last = c.Index
		}
	}
	for _, c := range cmds {
		if c.Skipped || c.Status == 0 || c.Index == last {
			continue
		}
		out = append(out, c.Index)
	}
	return out
}

// binOf is a display aid: the program name of a piece of shell text.
func binOf(text string) string {
	t := strings.TrimSpace(text)
	for {
		// drop leading NAME=value assignments
		i := strings.IndexAny(t, " \t")
		if i <= 0 || !strings.Contains(t[:i], "=") {
			break
		}
		t = strings.TrimSpace(t[i:])
	}
	if i := strings.IndexAny(t, " \t\n"); i > 0 {
		t = t[:i]
	}
	return t
}

func ms(d time.Duration) int64 { return d.Milliseconds() }
