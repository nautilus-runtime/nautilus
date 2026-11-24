package main

import (
	"errors"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"time"

	"golang.org/x/sys/unix"
)

// The guest keeps the original init's descriptors for its lifetime. An attach
// connection owns a subscription, never the console or the init process.
type processIO struct {
	mu       sync.Mutex
	input    *os.File
	terminal *os.File
	outputs  map[string]*os.File
}

func (p *processIO) close() {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.input != nil {
		p.input.Close()
		p.input = nil
	}
	if p.terminal != nil {
		p.terminal.Close()
		p.terminal = nil
	}
	for _, f := range p.outputs {
		f.Close()
	}
}
func (p *processIO) write(b []byte) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.input == nil {
		return errors.New("stdin is closed")
	}
	p.input.SetWriteDeadline(time.Now().Add(3 * time.Second))
	_, err := p.input.Write(b)
	return err
}
func (p *processIO) closeInput() error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.input == nil {
		return nil
	}
	var err error
	if p.terminal != nil {
		p.input.SetWriteDeadline(time.Now().Add(3 * time.Second))
		_, err = p.input.Write([]byte{4})
	}
	p.input.Close()
	p.input = nil
	return err
}
func (p *processIO) resize(width, height uint16) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	if p.terminal == nil {
		return errors.New("process has no terminal")
	}
	return controlFD(p.terminal, func(fd uintptr) error {
		return unix.IoctlSetWinsize(int(fd), unix.TIOCSWINSZ, &unix.Winsize{Col: width, Row: height})
	})
}

// File.Fd changes pollable descriptors back to blocking mode. Keep the PTY
// nonblocking so backpressure and cancellation can honor I/O deadlines.
func controlFD(f *os.File, fn func(uintptr) error) error {
	raw, err := f.SyscallConn()
	if err != nil {
		return err
	}
	var result error
	if err = raw.Control(func(fd uintptr) { result = fn(fd) }); err != nil {
		return err
	}
	return result
}
func dupFile(f *os.File, name string) (*os.File, error) {
	var fd int
	err := controlFD(f, func(source uintptr) error {
		var err error
		fd, err = unix.FcntlInt(source, unix.F_DUPFD_CLOEXEC, 0)
		return err
	})
	if err != nil {
		return nil, err
	}
	return os.NewFile(uintptr(fd), name), nil
}

// Receive the console allocated by runc in the workload's private devpts.
func receiveConsole(listener *net.UnixListener) (*os.File, error) {
	listener.SetDeadline(time.Now().Add(3 * time.Second))
	conn, err := listener.AcceptUnix()
	if err != nil {
		return nil, err
	}
	defer conn.Close()
	conn.SetReadDeadline(time.Now().Add(3 * time.Second))
	data := make([]byte, 4096)
	oob := make([]byte, unix.CmsgSpace(4))
	_, n, flags, _, err := conn.ReadMsgUnix(data, oob)
	if err != nil {
		return nil, err
	}
	msgs, err := unix.ParseSocketControlMessage(oob[:n])
	if err != nil || len(msgs) != 1 {
		return nil, errors.New("invalid console control message")
	}
	fds, err := unix.ParseUnixRights(&msgs[0])
	if err != nil {
		return nil, err
	}
	if flags&unix.MSG_CTRUNC != 0 || len(fds) != 1 {
		for _, fd := range fds {
			unix.Close(fd)
		}
		return nil, errors.New("expected one console descriptor")
	}
	fd := fds[0]
	unix.CloseOnExec(fd)
	if err := unix.SetNonblock(fd, true); err != nil {
		unix.Close(fd)
		return nil, err
	}
	return os.NewFile(uintptr(fd), "runc-console"), nil
}

func launchIO(cmd *exec.Cmd, dir, id string, tty, stdin bool) (*processIO, int, error) {
	p := &processIO{outputs: map[string]*os.File{}}
	success := false
	defer func() {
		if !success {
			p.close()
		}
	}()
	var listener *net.UnixListener
	if tty {
		socket := filepath.Join(dir, "console.sock")
		var err error
		listener, err = net.ListenUnix("unix", &net.UnixAddr{Name: socket, Net: "unix"})
		if err != nil {
			return nil, 0, err
		}
		defer listener.Close()
		cmd.Args = append(cmd.Args, "--console-socket", socket)
	} else {
		for _, name := range []string{"stdout", "stderr"} {
			reader, writer, err := os.Pipe()
			if err != nil {
				return nil, 0, err
			}
			defer writer.Close()
			p.outputs[name] = reader
			if name == "stdout" {
				cmd.Stdout = writer
			} else {
				cmd.Stderr = writer
			}
		}
		if stdin {
			reader, writer, err := os.Pipe()
			if err != nil {
				return nil, 0, err
			}
			defer reader.Close()
			cmd.Stdin = reader
			p.input = writer
		}
	}
	cmd.Args = append(cmd.Args, id)
	if err := cmd.Run(); err != nil {
		b, _ := os.ReadFile(filepath.Join(dir, "runtime.log"))
		return nil, 0, fmt.Errorf("runc: %w: %.2048s", err, b)
	}
	if tty {
		master, err := receiveConsole(listener)
		if err != nil {
			return nil, 0, err
		}
		p.terminal = master
		out, err := dupFile(master, "stdout")
		if err != nil {
			return nil, 0, err
		}
		p.outputs["stdout"] = out
		if stdin {
			p.input, err = dupFile(master, "stdin")
			if err != nil {
				return nil, 0, err
			}
		}
	}
	raw, err := os.ReadFile(filepath.Join(dir, "pid"))
	if err != nil {
		return nil, 0, err
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(raw)))
	if err != nil || pid < 2 {
		return nil, 0, errors.New("invalid runc PID")
	}
	success = true
	return p, pid, nil
}
