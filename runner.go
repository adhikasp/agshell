package main

import (
	"bytes"
	"io"
	"os"
	"os/exec"
	"strings"
	"sync"
	"syscall"
	"time"

	"mvdan.cc/sh/v3/syntax"
)

const stderrCap = 2048

// Runner executes a parsed script one element at a time, supervising every
// process it starts. bash still evaluates each simple command — agshell never
// reimplements words, globbing, quoting or arithmetic — but *it* owns the
// topology: which stage of which pipeline starts when, with which fds, and how
// it ended.
type Runner struct {
	src     string
	timeout time.Duration
	cmds    []Cmd
	sink    *limitedBuffer
	pipelines int
}

func NewRunner(src string, timeout time.Duration, maxStdout int) *Runner {
	return &Runner{
		src:     src,
		timeout: timeout,
		sink:    &limitedBuffer{limit: maxStdout},
	}
}

// Run parses and executes. Any parse error is reported, not guessed at.
func (r *Runner) Run() Result {
	start := time.Now()
	f, err := syntax.NewParser().Parse(strings.NewReader(r.src), "")
	if err != nil {
		return Result{
			Class:      "parse_error",
			Exit:       2,
			DurationMs: ms(time.Since(start)),
			Error:      err.Error(),
		}
	}

	r.cmds = nil
	status := 0
	for _, st := range f.Stmts {
		status = r.runStmt(st, r.sink)
	}

	res := Result{
		Exit:        status,
		DurationMs:  ms(time.Since(start)),
		Commands:    r.cmds,
		Stdout:      r.sink.String(),
		StdoutBytes: r.sink.total,
	}
	res.StdoutTruncated = r.sink.truncated
	res.Class = classify(r.cmds, status)
	if status == 0 {
		// only meaningful when the exit code itself said "fine"
		res.HiddenFailures = hidden(r.cmds, status)
	}
	res.OpaqueSubstitution = strings.Contains(r.src, "$(") || strings.Contains(r.src, "`")
	if res.Class == "ok" && errLine.MatchString(res.Stdout) {
		res.Class = "ok_noisy"
	}
	return res
}

// ---------------------------------------------------------------- execution

func (r *Runner) runStmt(st *syntax.Stmt, sink io.Writer) int {
	if st == nil || st.Cmd == nil {
		return r.delegate(st, sink)
	}
	// Anything that changes control flow or runs in its own process/scope is
	// handed to bash whole. Correctness beats visibility; the envelope marks it.
	if st.Negated || st.Background || st.Coprocess || len(st.Redirs) > 0 {
		return r.delegate(st, sink)
	}

	switch c := st.Cmd.(type) {
	case *syntax.CallExpr:
		if len(c.Assigns) > 0 && len(c.Args) == 0 {
			return r.delegate(st, sink) // bare assignment: no process
		}
		return r.simple(st, sink, 0, 0, false)
	case *syntax.BinaryCmd:
		switch c.Op {
		case syntax.Pipe:
			return r.pipeline(c, sink)
		case syntax.AndStmt:
			s := r.runStmt(c.X, sink)
			if s != 0 {
				r.skip(c.Y)
				return s
			}
			return r.runStmt(c.Y, sink)
		case syntax.OrStmt:
			s := r.runStmt(c.X, sink)
			if s == 0 {
				r.skip(c.Y)
				return 0
			}
			return r.runStmt(c.Y, sink)
		}
	}
	return r.delegate(st, sink)
}

// simple runs one simple command in its own process.
func (r *Runner) simple(st *syntax.Stmt, sink io.Writer, pipeline, stage int, delegated bool) int {
	text := textOf(r.src, st)
	idx := len(r.cmds)
	p := &proc{stderr: &bytes.Buffer{}}
	cmd := exec.Command("/bin/bash", "-c", text)
	cmd.Stdout = sink
	cmd.Stderr = p.stderr
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	p.cmd = cmd

	if err := cmd.Start(); err != nil {
		r.cmds = append(r.cmds, Cmd{
			Index: idx, Text: text, Bin: binOf(text), Status: 127,
			PipelineID: pipeline, Stage: stage, Delegated: delegated,
			Stderr: err.Error(),
		})
		return 127
	}
	status := r.wait(p)
	r.cmds = append(r.cmds, Cmd{
		Index: idx, Text: text, Bin: binOf(text), Status: status,
		Ms: ms(p.dur), TimedOut: p.timedOut, Stderr: head(p.stderr.String()),
		PipelineID: pipeline, Stage: stage, Delegated: delegated,
	})
	return status
}

// delegate hands a whole statement to bash, recording that we could not see
// inside it. Used for subshells, loops, conditionals, negation and background.
func (r *Runner) delegate(st *syntax.Stmt, sink io.Writer) int {
	if st == nil || st.Cmd == nil {
		return 0
	}
	return r.simple(st, sink, 0, 0, true)
}

// pipeline is the reason this exists: every stage is a child agshell started
// on purpose, so every stage's status is known even when bash would have
// reported only the last one.
func (r *Runner) pipeline(b *syntax.BinaryCmd, sink io.Writer) int {
	stages := pipeStages(b)
	r.pipelines++
	pid := r.pipelines
	procs := make([]*proc, len(stages))
	texts := make([]string, len(stages))

	var prevRead *os.File
	for i, st := range stages {
		texts[i] = textOf(r.src, st)
		var out io.Writer = sink
		var writeEnd, nextRead *os.File

		if i < len(stages)-1 {
			pr, pw, err := os.Pipe()
			if err != nil {
				prevRead = nil
				continue
			}
			writeEnd, nextRead = pw, pr
			out = pw
		}

		p := &proc{stderr: &bytes.Buffer{}}
		cmd := exec.Command("/bin/bash", "-c", texts[i])
		cmd.Stdin = prevRead
		cmd.Stdout = out
		cmd.Stderr = p.stderr
		cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
		p.cmd = cmd

		if err := cmd.Start(); err != nil {
			p.startErr = err
		} else {
			procs[i] = p
		}
		if prevRead != nil {
			prevRead.Close()
		}
		if writeEnd != nil {
			writeEnd.Close()
		}
		prevRead = nextRead
	}
	if prevRead != nil {
		prevRead.Close()
	}

	statuses := make([]int, len(stages))
	var wg sync.WaitGroup
	for i := range procs {
		if procs[i] == nil {
			statuses[i] = 127
			continue
		}
		wg.Add(1)
		go func(i int) {
			defer wg.Done()
			statuses[i] = r.wait(procs[i])
		}(i)
	}
	wg.Wait()

	for i := range stages {
		idx := len(r.cmds)
		c := Cmd{
			Index: idx, Text: texts[i], Bin: binOf(texts[i]),
			Status: statuses[i], PipelineID: pid, Stage: i,
		}
		if procs[i] != nil {
			c.Ms = ms(procs[i].dur)
			c.TimedOut = procs[i].timedOut
			c.Stderr = head(procs[i].stderr.String())
		} else {
			c.Stderr = "start failed"
		}
		r.cmds = append(r.cmds, c)
	}
	return statuses[len(statuses)-1] // bash reports the last stage
}

// skip records statements && / || prevented from running.
func (r *Runner) skip(st *syntax.Stmt) {
	if st == nil {
		return
	}
	r.cmds = append(r.cmds, Cmd{
		Index: len(r.cmds), Text: textOf(r.src, st), Status: -1, Skipped: true,
	})
}

// ------------------------------------------------------------------ process

type proc struct {
	cmd      *exec.Cmd
	stderr   *bytes.Buffer
	dur      time.Duration
	timedOut bool
	startErr error
}

// wait reaps one child, killing its whole process group on timeout. The group
// is the point: `bash -c "a | b"` in a pipeline stage must not outlive us.
func (r *Runner) wait(p *proc) int {
	start := time.Now()
	done := make(chan error, 1)
	go func() { done <- p.cmd.Wait() }()

	if r.timeout > 0 {
		timer := time.NewTimer(r.timeout)
		defer timer.Stop()
		select {
		case err := <-done:
			p.dur = time.Since(start)
			return statusOf(err)
		case <-timer.C:
			p.timedOut = true
			_ = syscall.Kill(-p.cmd.Process.Pid, syscall.SIGKILL)
			err := <-done
			p.dur = time.Since(start)
			_ = err
			return 137
		}
	}
	err := <-done
	p.dur = time.Since(start)
	return statusOf(err)
}

func statusOf(err error) int {
	if err == nil {
		return 0
	}
	if ee, ok := err.(*exec.ExitError); ok {
		if ws, ok := ee.Sys().(syscall.WaitStatus); ok {
			if ws.Signaled() {
				return 128 + int(ws.Signal())
			}
			return ws.ExitStatus()
		}
		return 1
	}
	return 127
}

// ------------------------------------------------------------------- helpers

func textOf(src string, n syntax.Node) string {
	if n == nil {
		return ""
	}
	s, e := int(n.Pos().Offset()), int(n.End().Offset())
	if s < 0 || e > len(src) || s >= e {
		return ""
	}
	return src[s:e]
}

func pipeStages(b *syntax.BinaryCmd) []*syntax.Stmt {
	return append(flatten(b.X), flatten(b.Y)...)
}

func flatten(st *syntax.Stmt) []*syntax.Stmt {
	if st == nil {
		return nil
	}
	if bb, ok := st.Cmd.(*syntax.BinaryCmd); ok && bb.Op == syntax.Pipe {
		return append(flatten(bb.X), flatten(bb.Y)...)
	}
	return []*syntax.Stmt{st}
}

func head(s string) string {
	if len(s) > stderrCap {
		return s[:stderrCap] + "…"
	}
	return s
}

type limitedBuffer struct {
	buf       bytes.Buffer
	limit     int
	total     int
	truncated bool
}

func (l *limitedBuffer) Write(p []byte) (int, error) {
	l.total += len(p)
	room := l.limit - l.buf.Len()
	if room <= 0 {
		l.truncated = true
		return len(p), nil
	}
	if len(p) > room {
		l.buf.Write(p[:room])
		l.truncated = true
		return len(p), nil
	}
	return l.buf.Write(p)
}

func (l *limitedBuffer) String() string { return l.buf.String() }
