package main

import (
	"testing"
	"time"
)

func run(t *testing.T, script string) Result {
	t.Helper()
	return NewRunner(script, 5*time.Second, 1<<16).Run()
}

// The headline claim: bash says 0 for `false; echo hi; true`, and the failure
// is gone. agshell keeps it.
func TestChainKeepsHiddenFailure(t *testing.T) {
	r := run(t, "false; echo hi; true")
	if r.Exit != 0 {
		t.Fatalf("bash-compatible exit should be 0, got %d", r.Exit)
	}
	if r.Class != "chain_partial" {
		t.Fatalf("class = %q, want chain_partial", r.Class)
	}
	if len(r.HiddenFailures) != 1 || r.HiddenFailures[0] != 0 {
		t.Fatalf("hidden failures = %v, want [0]", r.HiddenFailures)
	}
	if len(r.Commands) != 3 {
		t.Fatalf("expected 3 commands, got %d", len(r.Commands))
	}
	if r.Commands[0].Status != 1 || r.Commands[1].Status != 0 || r.Commands[2].Status != 0 {
		t.Fatalf("statuses = %d/%d/%d", r.Commands[0].Status, r.Commands[1].Status, r.Commands[2].Status)
	}
	if r.Stdout != "hi\n" {
		t.Fatalf("stdout = %q", r.Stdout)
	}
}

// Second headline claim: a failing stage before the last one is invisible to
// bash without pipefail, and invisible to pipefail when the chain has more
// than one pipeline in it. agshell reports stages per pipeline.
func TestPipelineStages(t *testing.T) {
	r := run(t, "false | head -1")
	if r.Exit != 0 {
		t.Fatalf("bash-compatible exit should be 0, got %d", r.Exit)
	}
	if r.Class != "stage_failed" {
		t.Fatalf("class = %q, want stage_failed", r.Class)
	}
	if len(r.Commands) != 2 {
		t.Fatalf("expected 2 stages, got %d", len(r.Commands))
	}
	if r.Commands[0].Status != 1 {
		t.Fatalf("stage 0 status = %d, want 1", r.Commands[0].Status)
	}
	if r.Commands[1].Status != 0 {
		t.Fatalf("stage 1 status = %d, want 0", r.Commands[1].Status)
	}
	if r.Commands[0].PipelineID == 0 || r.Commands[0].PipelineID != r.Commands[1].PipelineID {
		t.Fatalf("stages not grouped: %d / %d", r.Commands[0].PipelineID, r.Commands[1].PipelineID)
	}
}

func TestPipelineDataFlows(t *testing.T) {
	r := run(t, "printf 'b\\na\\nc\\n' | sort | head -2")
	if r.Class != "ok" {
		t.Fatalf("class = %q, want ok", r.Class)
	}
	if r.Stdout != "a\nb\n" {
		t.Fatalf("stdout = %q, want %q", r.Stdout, "a\nb\n")
	}
	if len(r.Commands) != 3 {
		t.Fatalf("expected 3 stages, got %d", len(r.Commands))
	}
}

func TestAndOrShortCircuit(t *testing.T) {
	r := run(t, "true && echo yes || echo no")
	if r.Class != "ok" || r.Stdout != "yes\n" {
		t.Fatalf("class=%q stdout=%q", r.Class, r.Stdout)
	}
	// executed: true, echo yes; recorded as skipped: echo no
	if len(r.Commands) != 3 {
		t.Fatalf("expected 3 entries (2 run + 1 skipped), got %d", len(r.Commands))
	}
	if !r.Commands[2].Skipped || r.Commands[2].Status != -1 {
		t.Fatalf("third entry should be skipped, got %+v", r.Commands[2])
	}

	r = run(t, "false && echo yes")
	if r.Exit != 1 || r.Class != "failed" {
		t.Fatalf("exit=%d class=%q", r.Exit, r.Class)
	}
	skipped := false
	for _, c := range r.Commands {
		if c.Skipped {
			skipped = true
		}
	}
	if !skipped {
		t.Fatal("the && right side should be recorded as skipped")
	}
}

func TestFailureIsFailed(t *testing.T) {
	r := run(t, "exit 3")
	if r.Class != "failed" || r.Exit != 3 {
		t.Fatalf("class=%q exit=%d", r.Class, r.Exit)
	}
}

func TestTimeout(t *testing.T) {
	r := NewRunner("sleep 30", 300*time.Millisecond, 1<<16).Run()
	if r.Class != "timeout" {
		t.Fatalf("class = %q, want timeout", r.Class)
	}
	if !r.Commands[0].TimedOut {
		t.Fatal("command should be marked timed_out")
	}
}

func TestOkNoisy(t *testing.T) {
	// exits 0 but prints something that reads like a failure
	r := run(t, "printf 'fatal: changed nothing\\n'")
	if r.Exit != 0 {
		t.Fatalf("exit = %d", r.Exit)
	}
	if r.Class != "ok_noisy" {
		t.Fatalf("class = %q, want ok_noisy", r.Class)
	}
}

func TestSubstitutionIsMarked(t *testing.T) {
	r := run(t, "echo $(false) done")
	if !r.OpaqueSubstitution {
		t.Fatal("command substitution should be flagged as opaque")
	}
}

func TestParseError(t *testing.T) {
	r := run(t, "if then")
	if r.Class != "parse_error" || r.Exit != 2 {
		t.Fatalf("class=%q exit=%d", r.Class, r.Exit)
	}
}

func TestStderrPerStage(t *testing.T) {
	r := run(t, "echo oops >&2 | cat")
	if len(r.Commands) != 2 {
		t.Fatalf("expected 2 stages, got %d", len(r.Commands))
	}
	if r.Commands[0].Stderr == "" {
		t.Fatal("stage 0 stderr should be captured separately from stage 1")
	}
}
