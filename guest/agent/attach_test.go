//go:build linux

package main

import (
	"context"
	"encoding/binary"
	"errors"
	"io"
	"os"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestAttachDrainsOutputAcrossInitExit(t *testing.T) {
	for _, tty := range []bool{false, true} {
		name := "pipe-closes-before-exit-observation"
		if tty {
			name = "resize-arrives-after-descriptor-cleanup"
		}
		t.Run(name, func(t *testing.T) {
			r, stdin, err := os.Pipe()
			if err != nil {
				t.Fatal(err)
			}
			defer stdin.Close()
			// The last reader can disappear before Wait4 observes the exit.
			r.Close()
			file, err := os.CreateTemp(t.TempDir(), "wire")
			if err != nil {
				t.Fatal(err)
			}
			defer file.Close()
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()
			c := &container{
				io:         &processIO{input: stdin},
				initExited: make(chan struct{}),
				done:       make(chan struct{}),
				exited:     status{ExitCode: 37},
			}
			if _, err := (attachmentWriter{hub: &c.attach, stream: "stdout"}).Write([]byte("startup")); err != nil {
				t.Fatal(err)
			}
			exit := func() {
				close(c.initExited)
				c.io.close()
				close(c.done)
			}
			wire := &streamWire{f: file, cancel: cancel}
			response := c.attachInit(ctx, wire, &streamOptions{Stdin: true, Stdout: true, TTY: tty}, func(input streamInput) error {
				if tty {
					exit()
					if err := input.resize(80, 24); err != nil {
						return err
					}
				} else {
					// Delay the exit observation while the closed pipe returns EPIPE.
					timer := time.AfterFunc(10*time.Millisecond, exit)
					defer timer.Stop()
				}
				return input.write([]byte("late input"))
			})
			if !response.OK || response.ExitCode != 37 {
				t.Fatalf("attach outcome: %+v", response)
			}
			if _, err := file.Seek(0, io.SeekStart); err != nil {
				t.Fatal(err)
			}
			wireBytes, err := io.ReadAll(file)
			if err != nil {
				t.Fatal(err)
			}
			if len(wireBytes) != 12 || wireBytes[0] != channelOutput ||
				binary.BigEndian.Uint32(wireBytes[1:5]) != 7 || string(wireBytes[5:]) != "startup" {
				t.Fatalf("output frame: %q", wireBytes)
			}
		})
	}
}

func TestAttachKeepsRealInputErrors(t *testing.T) {
	c := &container{initExited: make(chan struct{})}
	ctx := context.Background()
	if err := c.applyAttachInput(ctx, func() error { return unix.EPIPE }); !errors.Is(err, unix.EPIPE) {
		t.Fatalf("still-running init should report broken pipe, got %v", err)
	}
	if err := c.applyAttachInput(ctx, func() error {
		close(c.initExited)
		return unix.ENOTTY
	}); !errors.Is(err, unix.ENOTTY) {
		t.Fatalf("terminal I/O error was lost: %v", err)
	}
}
