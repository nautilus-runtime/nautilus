package main

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"os"
	"strconv"
	"sync"
	"time"
)

const maxChunk = 65536
const (
	channelInput byte = iota
	channelOutput
	channelErrorOutput
	channelFailure
	channelResize
	channelCloseInput
	channelExit
)

type streamOptions struct {
	Stdin  bool   `json:"stdin,omitempty"`
	Stdout bool   `json:"stdout,omitempty"`
	Stderr bool   `json:"stderr,omitempty"`
	TTY    bool   `json:"tty,omitempty"`
	Port   uint16 `json:"port,omitempty"`
}
type packet struct {
	tag  byte
	data []byte
}
type streamWire struct {
	f      *os.File
	mu     sync.Mutex
	cancel context.CancelFunc
}

func (w *streamWire) send(tag byte, b []byte) error {
	if len(b) > maxChunk {
		return errors.New("stream chunk too large")
	}
	w.mu.Lock()
	defer w.mu.Unlock()
	w.f.SetWriteDeadline(time.Now().Add(3 * time.Second))
	var h [5]byte
	h[0] = tag
	binary.BigEndian.PutUint32(h[1:], uint32(len(b)))
	if _, err := w.f.Write(h[:]); err != nil {
		w.cancel()
		return err
	}
	if len(b) > 0 {
		if _, err := w.f.Write(b); err != nil {
			w.cancel()
			return err
		}
	}
	return nil
}

type channelWriter struct {
	w   *streamWire
	tag byte
}

func (w channelWriter) Write(b []byte) (int, error) {
	if err := w.w.send(w.tag, b); err != nil {
		return 0, err
	}
	return len(b), nil
}
func (w *streamWire) writer(tag byte, enabled bool) io.Writer {
	if enabled {
		return channelWriter{w, tag}
	}
	return io.Discard
}

type streamInput struct {
	write  func([]byte) error
	eof    func() error
	resize func(uint16, uint16) error
}

func (w *streamWire) read(in streamInput) {
	defer w.cancel()
	var h [5]byte
	for {
		if _, err := io.ReadFull(w.f, h[:]); err != nil {
			return
		}
		n := binary.BigEndian.Uint32(h[1:])
		tag := h[0]
		if n > maxChunk || !(tag == channelInput && in.write != nil || tag == channelCloseInput && n == 0 && in.eof != nil || tag == channelResize && n == 4 && in.resize != nil) {
			w.send(channelFailure, []byte("invalid stream input frame"))
			return
		}
		b := make([]byte, n)
		if _, err := io.ReadFull(w.f, b); err != nil {
			return
		}
		var err error
		switch tag {
		case channelInput:
			err = in.write(b)
		case channelCloseInput:
			err = in.eof()
			in.write = nil
			in.eof = nil
		case channelResize:
			width, height := binary.BigEndian.Uint16(b[:2]), binary.BigEndian.Uint16(b[2:])
			if width == 0 || height == 0 {
				err = errors.New("invalid terminal size")
			} else {
				err = in.resize(width, height)
			}
		}
		if err != nil {
			w.send(channelFailure, []byte(err.Error()))
			return
		}
	}
}
func (p *processIO) inputFor(o *streamOptions, closeStdin bool) streamInput {
	var in streamInput
	if o.Stdin {
		in.write = p.write
		if closeStdin {
			in.eof = p.closeInput
		} else {
			in.eof = func() error { return nil }
		}
	}
	if o.TTY {
		in.resize = p.resize
	}
	return in
}

func (s *server) stream(f *os.File, q request) {
	acknowledged := false
	reply := func(r response) error {
		r.Version, r.ID = 1, q.ID
		f.SetWriteDeadline(time.Now().Add(3 * time.Second))
		return writeFrame(f, r)
	}
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	w := &streamWire{f: f, cancel: cancel}
	ack := func(in streamInput) error {
		if err := reply(success(status{})); err != nil {
			return err
		}
		acknowledged = true
		go w.read(in)
		return nil
	}
	o := q.Stream
	if o == nil || q.Timeout != nil || q.Offset != nil || q.Limit != nil ||
		q.Op == "execStream" && !validArgv(q.Argv) || q.Op != "execStream" && q.Argv != nil ||
		q.Op == "portForward" && (o.Port == 0 || q.Container != s.Sandbox || o.Stdin || o.Stdout || o.Stderr || o.TTY) ||
		q.Op != "portForward" && (o.Port != 0 || !o.Stdin && !o.Stdout && !o.Stderr || o.TTY && o.Stderr) {
		reply(fail("invalid", "invalid stream options"))
		return
	}
	select {
	case s.execSlots <- struct{}{}:
		defer func() { <-s.execSlots }()
	default:
		reply(fail("limit", "too many active streams"))
		return
	}
	var res response
	if q.Op == "portForward" {
		res = forwardPort(ctx, w, o.Port, ack)
	} else {
		s.mu.Lock()
		c := s.containers[q.Container]
		s.mu.Unlock()
		if c == nil {
			reply(fail("not_found", "container not found"))
			return
		}
		c.mu.Lock()
		if c.current().State != "running" {
			c.mu.Unlock()
			reply(fail("conflict", "container is not running"))
			return
		}
		if q.Op == "attach" && (o.TTY != c.p.Terminal || o.Stdin && !c.p.OpenStdin) {
			c.mu.Unlock()
			reply(fail("invalid", "attach options conflict with init configuration"))
			return
		}
		c.execs.Add(1)
		c.mu.Unlock()
		defer c.execs.Done()
		if q.Op == "execStream" {
			res = c.runExec(ctx, q.Argv, o.TTY, o.Stdin, func(p *processIO) error { return ack(p.inputFor(o, true)) }, w.writer(channelOutput, o.Stdout), w.writer(channelErrorOutput, o.Stderr))
		} else {
			res = c.attachInit(ctx, w, o, ack)
		}
	}
	if !acknowledged {
		reply(res)
		return
	}
	if ctx.Err() != nil {
		return
	}
	if !res.OK {
		w.send(channelFailure, []byte(res.Message))
		return
	}
	var code [4]byte
	binary.BigEndian.PutUint32(code[:], uint32(int32(res.ExitCode)))
	w.send(channelExit, code[:])
}

func forwardPort(ctx context.Context, w *streamWire, port uint16, ack func(streamInput) error) response {
	var conn net.Conn
	var err error
	for _, host := range []string{"127.0.0.1", "::1"} {
		conn, err = net.DialTimeout("tcp", net.JoinHostPort(host, strconv.Itoa(int(port))), 2*time.Second)
		if err == nil {
			break
		}
	}
	if err != nil {
		return fail("runtime", fmt.Errorf("connect to guest port %d: %w", port, err))
	}
	defer conn.Close()
	stop := context.AfterFunc(ctx, func() { conn.Close() })
	defer stop()
	tcp := conn.(*net.TCPConn)
	if err := ack(streamInput{write: func(b []byte) error {
		conn.SetWriteDeadline(time.Now().Add(3 * time.Second))
		_, err := conn.Write(b)
		return err
	}, eof: tcp.CloseWrite}); err != nil {
		return fail("runtime", err)
	}
	if _, err = io.Copy(w.writer(channelOutput, true), conn); err != nil {
		return fail("runtime", err)
	}
	return success(status{})
}
