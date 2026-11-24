package main

import (
	"bufio"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"golang.org/x/sys/unix"
)

var invalidBundle = errors.New("invalid OCI bundle")

type logState struct {
	sync.Mutex
	err error
}

type container struct {
	logState logState
	mu       sync.Mutex
	execs    sync.WaitGroup
	waitErr  error
	id, dir  string
	s        status
	p        process
	pidfd    int
	done     chan struct{}
	exited   status // published by closing done
	log      *os.File
}

func (c *container) current() status {
	select {
	case <-c.done:
		s := c.s
		s.State, s.Finished, s.ExitCode = "exited", c.exited.Finished, c.exited.ExitCode
		return s
	default:
		return c.s
	}
}
func runc(ctx context.Context, args ...string) *exec.Cmd {
	cmd := exec.CommandContext(ctx, "/bin/runc", append([]string{"--root", runtimeRoot}, args...)...)
	cmd.Env = []string{"PATH=/bin", "HOME=/", "XDG_RUNTIME_DIR=/run"}
	return cmd
}
func command(args ...string) error {
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	b, err := runc(ctx, args...).CombinedOutput()
	if err != nil {
		return fmt.Errorf("runc: %v: %.2048s", err, b)
	}
	return nil
}

// Stream records remain bounded even when an application never emits a newline.
func (c *container) copyLog(stream string, r *os.File, wg *sync.WaitGroup) {
	defer wg.Done()
	defer r.Close()
	reader := bufio.NewReaderSize(r, 16<<10)
	for {
		line, err := reader.ReadSlice('\n')
		if len(line) > 0 {
			flag := "F"
			if errors.Is(err, bufio.ErrBufferFull) {
				flag = "P"
			}
			if line[len(line)-1] == '\n' {
				line = line[:len(line)-1]
			}
			record := append([]byte(time.Now().UTC().Format(time.RFC3339Nano)+" "+stream+" "+flag+" "), line...)
			if _, e := c.log.Write(append(record, '\n')); e != nil {
				c.logState.Lock()
				c.logState.err = e
				c.logState.Unlock()
				io.Copy(io.Discard, reader)
				return
			}
		}
		if err != nil && !errors.Is(err, bufio.ErrBufferFull) {
			return
		}
	}
}
func (c *container) create() error {
	p, err := prepare(c.id, c.dir)
	if err != nil {
		return fmt.Errorf("%w: %w", invalidBundle, err)
	}
	c.p = p
	logpath := filepath.Join(share, "containers", c.id, "container.log")
	fd, err := unix.Open(logpath, unix.O_RDWR|unix.O_APPEND|unix.O_CREAT|unix.O_EXCL|unix.O_NOFOLLOW|unix.O_CLOEXEC, 0600)
	if err != nil {
		return err
	}
	c.log = os.NewFile(uintptr(fd), "container.log")
	complete := false
	defer func() {
		if !complete {
			c.log.Close()
			os.Remove(logpath)
		}
	}()
	outR, outW, err := os.Pipe()
	if err != nil {
		return err
	}
	defer outW.Close()
	errR, errW, err := os.Pipe()
	if err != nil {
		outR.Close()
		return err
	}
	defer errW.Close()
	var streams sync.WaitGroup
	streams.Add(2)
	go c.copyLog("stdout", outR, &streams)
	go c.copyLog("stderr", errR, &streams)
	pidfile := filepath.Join(c.dir, "pid")
	runtimeLog := filepath.Join(c.dir, "runtime.log")
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()
	cmd := runc(ctx, "--log", runtimeLog, "create", "--no-pivot", "--bundle", c.dir, "--pid-file", pidfile, c.id)
	cmd.Stdout, cmd.Stderr = outW, errW
	if err = cmd.Run(); err != nil {
		b, _ := os.ReadFile(runtimeLog)
		return fmt.Errorf("runc create: %w: %.2048s", err, b)
	}
	defer func() {
		if !complete {
			command("delete", "--force", c.id)
		}
	}()
	raw, err := os.ReadFile(pidfile)
	if err != nil {
		return err
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(raw)))
	if err != nil || pid < 2 {
		return errors.New("invalid runc PID")
	}
	c.pidfd, err = unix.PidfdOpen(pid, 0)
	if err != nil {
		return err
	}
	c.done = make(chan struct{})
	c.s = status{State: "created", Created: time.Now().UnixNano()}
	go func() {
		var ws unix.WaitStatus
		for {
			_, e := unix.Wait4(pid, &ws, 0, nil)
			if errors.Is(e, unix.EINTR) {
				continue
			}
			if e != nil {
				c.waitErr = fmt.Errorf("wait for init: %w", e)
				close(c.done)
				return
			}
			break
		}
		code := ws.ExitStatus()
		if ws.Signaled() {
			code = 128 + int(ws.Signal())
		}
		c.exited = status{ExitCode: code, Finished: time.Now().UnixNano()}
		streams.Wait()
		close(c.done)
	}()
	complete = true
	return nil
}
func (c *container) stop(grace time.Duration) error {
	if c.current().State == "exited" {
		return nil
	}
	signal := unix.SIGKILL
	if grace > 0 && c.current().State == "running" {
		signal = unix.SIGTERM
	}
	send := func(sig unix.Signal) error {
		err := unix.PidfdSendSignal(c.pidfd, sig, nil, 0)
		if errors.Is(err, unix.ESRCH) {
			return nil
		}
		return err
	}
	if err := send(signal); err != nil {
		return err
	}
	if signal == unix.SIGTERM {
		select {
		case <-c.done:
			return nil
		case <-time.After(grace):
		}
		if err := send(unix.SIGKILL); err != nil {
			return err
		}
	}
	select {
	case <-c.done:
		return nil
	case <-time.After(5 * time.Second):
		return errors.New("container exit not observed")
	}
}

func (s *server) dispatch(ctx context.Context, q request) response {
	useExec, useStop, readLog := q.Op == "exec", q.Op == "stop", q.Op == "readLog"
	switch q.Op {
	case "create", "start", "status", "exec", "stop", "remove", "readLog":
	default:
		return fail("invalid", "unknown operation")
	}
	if (useExec && (!validArgv(q.Argv) || q.Timeout == nil)) || (!useExec && q.Argv != nil) ||
		(q.Timeout != nil && ((!useExec && !useStop) || *q.Timeout < 0 || *q.Timeout > 300000)) ||
		(readLog && (q.Offset == nil || *q.Offset < 0 || q.Limit == nil || *q.Limit < 1 || *q.Limit > maxOutput)) || (!readLog && (q.Offset != nil || q.Limit != nil)) {
		return fail("invalid", "invalid operation arguments")
	}
	s.mu.Lock()
	c := s.containers[q.Container]
	if c == nil && q.Op == "create" {
		c = &container{id: q.Container, dir: filepath.Join("/run/nautilus", q.Container)}
		s.containers[q.Container] = c
	}
	s.mu.Unlock()
	if c == nil {
		if q.Op == "remove" || useStop {
			return success(status{})
		}
		return fail("not_found", "container not found")
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	forget := func() {
		s.mu.Lock()
		if s.containers[c.id] == c {
			delete(s.containers, c.id)
		}
		s.mu.Unlock()
	}
	if q.Op == "create" && c.s.State == "" {
		if err := c.create(); err != nil {
			os.RemoveAll(c.dir)
			c.s.State = "removed"
			forget()
			if errors.Is(err, invalidBundle) {
				return fail("invalid", err)
			}
			return fail("runtime", err)
		}
	}
	if c.s.State == "removed" || c.s.State == "" {
		if useStop || q.Op == "remove" {
			return success(status{})
		}
		return fail("not_found", "container not found")
	}
	current := c.current()
	if current.State == "exited" && c.waitErr != nil && q.Op != "remove" {
		return fail("runtime", c.waitErr)
	}
	switch q.Op {
	case "start":
		if current.Started != 0 {
			break
		}
		if current.State != "created" {
			return fail("conflict", "container already exited")
		}
		started := time.Now().UnixNano()
		if err := command("start", c.id); err != nil {
			return fail("runtime", err)
		}
		c.s.State, c.s.Started = "running", started
	case "exec":
		select {
		case s.execSlots <- struct{}{}:
			defer func() { <-s.execSlots }()
		default:
			return fail("limit", "too many active execs")
		}
		if current.State != "running" {
			return fail("conflict", "container is not running")
		}
		// Stop/remove may proceed while an exec is waiting for the workload.
		c.execs.Add(1)
		c.mu.Unlock()
		res := c.exec(ctx, q)
		c.execs.Done()
		c.mu.Lock()
		return res
	case "readLog":
		c.logState.Lock()
		e := c.logState.err
		c.logState.Unlock()
		if e != nil {
			return fail("runtime", e)
		}
		b := make([]byte, *q.Limit)
		n, err := c.log.ReadAt(b, *q.Offset)
		if err != nil && err != io.EOF {
			return fail("runtime", err)
		}
		off := *q.Offset + int64(n)
		return response{OK: true, Stdout: b[:n], NextOffset: &off}
	case "stop", "remove":
		var grace time.Duration
		if q.Timeout != nil {
			grace = time.Duration(*q.Timeout) * time.Millisecond
		}
		if err := c.stop(grace); err != nil {
			return fail("runtime", err)
		}
		if q.Op == "remove" {
			c.execs.Wait()
			if err := command("delete", "--force", c.id); err != nil {
				return fail("runtime", err)
			}
			if err := os.RemoveAll(c.dir); err != nil {
				return fail("runtime", err)
			}
			c.log.Close()
			unix.Close(c.pidfd)
			c.s.State = "removed"
			forget()
			return success(status{})
		}
	}
	return success(c.current())
}
