package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"io"
	"os"
	"path/filepath"
	"sync"
	"time"

	"golang.org/x/sys/unix"
)

type capture struct {
	buf      bytes.Buffer
	overflow bool
	cancel   context.CancelFunc
}

func (w *capture) Write(b []byte) (int, error) {
	n := len(b)
	if n > maxOutput-w.buf.Len() {
		b = b[:maxOutput-w.buf.Len()]
		w.overflow = true
		w.cancel()
	}
	w.buf.Write(b)
	return n, nil
}

func (c *container) exec(parent context.Context, q request) response {
	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	if *q.Timeout > 0 {
		var stop context.CancelFunc
		ctx, stop = context.WithTimeout(ctx, time.Duration(*q.Timeout)*time.Millisecond)
		defer stop()
	}
	stdout, stderr := &capture{cancel: cancel}, &capture{cancel: cancel}
	res := c.runExec(ctx, q.Argv, false, false, func(_ *processIO) error { return nil }, stdout, stderr)
	if stdout.overflow || stderr.overflow {
		return fail("limit", "exec output exceeds 256 KiB per stream")
	}
	if res.OK {
		res.Stdout, res.Stderr = stdout.buf.Bytes(), stderr.buf.Bytes()
	}
	return res
}

// As guest subreaper, wait for the exact process spawned by detached runc.
// Its private cgroup also covers descendants that outlive or detach from it.
func (c *container) runExec(ctx context.Context, argv []string, tty, stdin bool, ready func(*processIO) error, stdout, stderr io.Writer) response {
	dir, err := os.MkdirTemp(c.dir, "exec-")
	if err != nil {
		return fail("runtime", err)
	}
	defer os.RemoveAll(dir)
	group := filepath.Join("/sys/fs/cgroup/nautilus", c.id, filepath.Base(dir))
	if err = os.Mkdir(group, 0700); err != nil {
		return fail("runtime", err)
	}
	defer os.Remove(group)
	kill := func() error { return os.WriteFile(filepath.Join(group, "cgroup.kill"), []byte("1"), 0600) }
	if _, err = os.Stat(filepath.Join(group, "cgroup.kill")); err != nil {
		return fail("runtime", "exec requires cgroup v2 cgroup.kill")
	}
	defer kill()
	p := c.p
	p.Args = argv
	p.Terminal = tty
	data, err := json.Marshal(p)
	if err != nil {
		return fail("invalid", err)
	}
	cfg := filepath.Join(dir, "process.json")
	if err = os.WriteFile(cfg, data, 0600); err != nil {
		return fail("runtime", err)
	}
	setup, stop := context.WithTimeout(ctx, 10*time.Second)
	defer stop()
	cmd := runc(setup, "--log", filepath.Join(dir, "runtime.log"), "exec", "--detach", "--pid-file", filepath.Join(dir, "pid"), "--cgroup", filepath.Base(dir), "--process", cfg)
	pio, pid, err := launchIO(cmd, dir, c.id, tty, stdin)
	if err != nil {
		if ctx.Err() != nil {
			return fail("timeout", ctx.Err())
		}
		return fail("runtime", err)
	}
	defer pio.close()
	if err = ready(pio); err != nil {
		kill()
	}
	var streams sync.WaitGroup
	for name, file := range pio.outputs {
		w := stdout
		if name == "stderr" {
			w = stderr
		}
		streams.Add(1)
		go func() { defer streams.Done(); io.Copy(w, file) }()
	}
	done := make(chan struct{})
	var ws unix.WaitStatus
	var waitErr error
	go func() {
		defer close(done)
		for {
			_, waitErr = unix.Wait4(pid, &ws, 0, nil)
			if !errors.Is(waitErr, unix.EINTR) {
				return
			}
		}
	}()
	select {
	case <-done:
	case <-ctx.Done():
		kill()
		<-done
	}
	if e := kill(); e != nil && !errors.Is(e, os.ErrNotExist) {
		pio.close()
		streams.Wait()
		return fail("runtime", e)
	}
	streams.Wait()
	if err != nil {
		return fail("runtime", err)
	}
	if ctx.Err() != nil {
		return fail("timeout", ctx.Err())
	}
	if waitErr != nil {
		return fail("runtime", waitErr)
	}
	code := ws.ExitStatus()
	if ws.Signaled() {
		code = 128 + int(ws.Signal())
	}
	return success(status{ExitCode: code})
}
