package main

import (
	"context"
	"errors"
	"sync"
)

type attachHub struct {
	sync.Mutex
	observers map[chan packet]*streamOptions
}
type attachmentWriter struct {
	hub    *attachHub
	stream string
}

func (w attachmentWriter) Write(b []byte) (int, error) {
	h := w.hub
	h.Lock()
	defer h.Unlock()
	var p packet
	for observer, options := range h.observers {
		if w.stream == "stdout" && !options.Stdout || w.stream == "stderr" && !options.Stderr {
			continue
		}
		if p.data == nil {
			p = packet{channelOutput, append([]byte(nil), b...)}
			if w.stream == "stderr" {
				p.tag = channelErrorOutput
			}
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
	h.observers[observer] = o
	h.Unlock()
	defer func() { h.Lock(); delete(h.observers, observer); h.Unlock() }()
	if o.Stdin && c.p.StdinOnce {
		defer c.io.closeInput()
	}
	if err := ack(c.io.inputFor(o, c.p.StdinOnce)); err != nil {
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
