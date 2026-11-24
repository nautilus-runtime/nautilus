package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"golang.org/x/sys/unix"
	"io"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"sync"
	"time"
)

// Each attached runc exec owns a guest cgroup. Killing the whole cgroup also
// reaches children that have changed their process group or closed stdio.
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

// runc can continue forwarding stdio from descendants after the requested
// process has exited. Its pidfd identifies that process throughout its lifetime.
func waitExecLeader(listener *net.UnixListener) error {
	conn, err := listener.AcceptUnix()
	if err != nil {
		return err
	}
	defer conn.Close()
	var payload [4096]byte
	rights := make([]byte, unix.CmsgSpace(4))
	_, n, flags, _, err := conn.ReadMsgUnix(payload[:], rights)
	if err != nil {
		return err
	}
	msgs, err := unix.ParseSocketControlMessage(rights[:n])
	if err != nil || len(msgs) != 1 || flags&unix.MSG_CTRUNC != 0 {
		return errors.New("invalid runc pidfd")
	}
	fds, err := unix.ParseUnixRights(&msgs[0])
	if err != nil || len(fds) != 1 {
		return errors.New("missing runc pidfd")
	}
	defer unix.Close(fds[0])
	poll := []unix.PollFd{{Fd: int32(fds[0]), Events: unix.POLLIN}}
	for {
		_, err = unix.Poll(poll, -1)
		if !errors.Is(err, unix.EINTR) {
			return err
		}
	}
}

func (c *container) exec(parent context.Context, q request) response {
	ctx, cancel := context.WithCancel(parent)
	defer cancel()
	if *q.Timeout > 0 {
		var stop context.CancelFunc
		ctx, stop = context.WithTimeout(ctx, time.Duration(*q.Timeout)*time.Millisecond)
		defer stop()
	}
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
	p := c.p
	p.Args = q.Argv
	data, err := json.Marshal(p)
	if err != nil {
		return fail("invalid", err)
	}
	cfg := filepath.Join(dir, "process.json")
	if err = os.WriteFile(cfg, data, 0600); err != nil {
		return fail("runtime", err)
	}
	// Keep the runc supervisor alive to reap the exec process and report its status.
	runctx, abort := context.WithCancel(context.Background())
	defer abort()
	runtimeLog := filepath.Join(dir, "runtime.log")
	socket := filepath.Join(dir, "pidfd.sock")
	listener, err := net.ListenUnix("unix", &net.UnixAddr{Name: socket, Net: "unix"})
	if err != nil {
		return fail("runtime", err)
	}
	defer listener.Close()
	cmd := runc(runctx, "--log", runtimeLog, "exec", "--pidfd-socket", socket, "--cgroup", filepath.Base(dir), "--process", cfg, c.id)
	stdout, stderr := &capture{cancel: cancel}, &capture{cancel: cancel}
	outR, outW, err := os.Pipe()
	if err != nil {
		return fail("runtime", err)
	}
	defer outR.Close()
	defer outW.Close()
	errR, errW, err := os.Pipe()
	if err != nil {
		return fail("runtime", err)
	}
	defer errR.Close()
	defer errW.Close()
	cmd.Stdout, cmd.Stderr = outW, errW
	if err = cmd.Start(); err != nil {
		return fail("runtime", err)
	}
	outW.Close()
	errW.Close()
	watched := make(chan struct{})
	go func() {
		defer close(watched)
		if waitExecLeader(listener) == nil {
			kill()
		}
	}()
	defer func() { listener.Close(); <-watched }()
	// Wait observes the runc supervisor independently of descendants holding
	// stdout/stderr open. Once the cgroup is empty, both pipes reach EOF.
	var streams sync.WaitGroup
	streams.Add(2)
	go func() { defer streams.Done(); io.Copy(stdout, outR) }()
	go func() { defer streams.Done(); io.Copy(stderr, errR) }()
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case err = <-done:
	case <-ctx.Done():
		// Cancellation can race runc placement. Keep killing until its supervisor
		// has exited (and once afterwards); bound a broken runtime's setup.
		timer := time.NewTimer(3 * time.Second)
		defer timer.Stop()
		tick := time.NewTicker(10 * time.Millisecond)
		defer tick.Stop()
	waiting:
		for {
			kill()
			select {
			case <-done:
				break waiting
			case <-tick.C:
			case <-timer.C:
				abort()
				<-done
				break waiting
			}
		}
	}
	if killErr := kill(); killErr != nil && !errors.Is(killErr, os.ErrNotExist) {
		outR.Close()
		errR.Close()
		streams.Wait()
		return fail("runtime", killErr)
	}
	streams.Wait()
	if stdout.overflow || stderr.overflow {
		return fail("limit", "exec output exceeds 256 KiB per stream")
	}
	if ctx.Err() != nil {
		return fail("timeout", ctx.Err())
	}
	code := 0
	if err != nil {
		var exit *exec.ExitError
		if !errors.As(err, &exit) {
			return fail("runtime", err)
		}
		code = exit.ExitCode()
		if b, _ := os.ReadFile(runtimeLog); bytes.Contains(b, []byte("level=error")) {
			return fail("runtime", string(b))
		}
	}
	return response{OK: true, status: status{ExitCode: code}, Stdout: stdout.buf.Bytes(), Stderr: stderr.buf.Bytes()}
}
