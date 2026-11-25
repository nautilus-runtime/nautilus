package main

import (
	"context"
	"errors"
	"sync"
	"time"

	"golang.org/x/sys/unix"
)

type attachHub struct {
	sync.Mutex
	observers map[chan packet]*streamOptions
	startup   [2][]byte
	claimed   [2]bool
}
type attachmentWriter struct {
	hub    *attachHub
	stream string
}

func (w attachmentWriter) Write(b []byte) (int, error) {
	h := w.hub
	h.Lock()
	defer h.Unlock()
	tag := channelOutput
	if w.stream == "stderr" {
		tag = channelErrorOutput
	}
	slot := tag - channelOutput
	if !h.claimed[slot] {
		const limit = 16 << 10
		h.startup[slot] = append(h.startup[slot], b[:min(len(b), limit-len(h.startup[slot]))]...)
	}
	var p packet
	for observer, options := range h.observers {
		if tag == channelOutput && !options.Stdout || tag == channelErrorOutput && !options.Stderr {
			continue
		}
		if p.data == nil {
			p = packet{tag, append([]byte(nil), b...)}
		}
		select {
		case observer <- p:
		default:
			delete(h.observers, observer)
			close(observer)
		}
	}
	return len(b), nil
}
func (c *container) attachInit(ctx context.Context, w *streamWire, o *streamOptions, ack func(streamInput) error) response {
	h := &c.attach
	observer := make(chan packet, 16)
	h.Lock()
	if h.observers == nil {
		h.observers = map[chan packet]*streamOptions{}
	}
	for slot, requested := range [...]bool{o.Stdout, o.Stderr} {
		if requested {
			h.claimed[slot] = true
			if len(h.startup[slot]) > 0 {
				observer <- packet{channelOutput + byte(slot), h.startup[slot]}
				h.startup[slot] = nil
			}
		}
	}
	h.observers[observer] = o
	h.Unlock()
	defer func() { h.Lock(); delete(h.observers, observer); h.Unlock() }()
	input := c.io.inputFor(o, c.p.StdinOnce)
	if write := input.write; write != nil {
		input.write = func(b []byte) error {
			return c.applyAttachInput(ctx, func() error { return write(b) })
		}
	}
	if eof := input.eof; eof != nil {
		input.eof = func() error { return c.applyAttachInput(ctx, eof) }
	}
	if resize := input.resize; resize != nil {
		input.resize = func(width, height uint16) error {
			return c.applyAttachInput(ctx, func() error { return resize(width, height) })
		}
	}
	if o.Stdin && c.p.StdinOnce {
		defer c.io.closeInput()
	}
	if err := ack(input); err != nil {
		return fail("runtime", err)
	}
	emit := func(p packet, ok bool) error {
		if !ok {
			return errors.New("attach client exceeded its output buffer")
		}
		return w.send(p.tag, p.data)
	}
	for {
		select {
		case <-ctx.Done():
			return fail("runtime", ctx.Err())
		case p, ok := <-observer:
			if err := emit(p, ok); err != nil {
				return fail("runtime", err)
			}
		case <-c.done:
			// Log readers stop publishing before the exit status becomes observable.
			for {
				select {
				case p, ok := <-observer:
					if err := emit(p, ok); err != nil {
						return fail("runtime", err)
					}
				default:
					if c.waitErr != nil {
						return fail("runtime", c.waitErr)
					}
					return success(c.exited)
				}
			}
		}
	}
}

func (c *container) applyAttachInput(ctx context.Context, apply func() error) error {
	select {
	case <-c.initExited:
		return nil
	default:
	}
	err := apply()
	switch {
	case errors.Is(err, errStdinClosed), errors.Is(err, errNoTerminal):
		// Final descriptor cleanup only happens after Wait4 publishes exit.
		select {
		case <-c.initExited:
			return nil
		default:
		}
	case errors.Is(err, unix.EPIPE), errors.Is(err, unix.EIO):
		// A pipe or PTY can lose its peer just before Wait4 reports exit.
		// Keep an actual I/O error if the init is still running.
		timer := time.NewTimer(250 * time.Millisecond)
		defer timer.Stop()
		select {
		case <-c.initExited:
			return nil
		case <-timer.C:
			select {
			case <-c.initExited:
				return nil
			default:
			}
		case <-ctx.Done():
		}
	}
	return err
}
