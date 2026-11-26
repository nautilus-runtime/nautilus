//go:build linux

package main

import (
	"bytes"
	"context"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"regexp"
	"sync"
	"time"
	"unicode/utf8"

	"golang.org/x/sys/unix"
)

const share = "/run/nautilus/share"
const runtimeRoot = "/run/nautilus/runc"
const maxOutput = 256 * 1024

var uuid = regexp.MustCompile("^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$")
var errResponseTooLong = errors.New("response too long")

func validID(s string) bool {
	return uuid.MatchString(s) && s != "00000000-0000-0000-0000-000000000000"
}

type identity struct {
	Version int    `json:"version"`
	Sandbox string `json:"sandboxId"`
	Nonce   string `json:"nonce"`
	Boot    string `json:"bootId"`
}
type request struct {
	Stream *streamOptions `json:"stream,omitempty"`
	identity
	ID        string   `json:"requestId"`
	Container string   `json:"containerId"`
	Op        string   `json:"op"`
	Argv      []string `json:"argv,omitempty"`
	Timeout   *int64   `json:"timeoutMs,omitempty"`
	Offset    *int64   `json:"offset,omitempty"`
	Limit     *int     `json:"limit,omitempty"`
}
type status struct {
	State    string `json:"state,omitempty"`
	Created  int64  `json:"createdAt,omitempty"`
	Started  int64  `json:"startedAt,omitempty"`
	Finished int64  `json:"finishedAt,omitempty"`
	ExitCode int    `json:"exitCode"`
}
type response struct {
	Version int    `json:"version"`
	ID      string `json:"requestId"`
	OK      bool   `json:"ok"`
	Code    string `json:"code,omitempty"`
	Message string `json:"message,omitempty"`
	status
	Stdout     []byte            `json:"stdout,omitempty"`
	Stderr     []byte            `json:"stderr,omitempty"`
	NextOffset *int64            `json:"nextOffset,omitempty"`
	Stats      *sample           `json:"stats,omitempty"`
	Containers map[string]sample `json:"containers,omitempty"`
}

func fail(code string, err any) response { return response{Code: code, Message: fmt.Sprint(err)} }
func success(s status) response          { return response{OK: true, status: s} }

type server struct {
	identity
	mu           sync.Mutex
	containers   map[string]*container
	execSlots    chan struct{}
	statsMu      sync.Mutex
	filesystemMu sync.Mutex
	cpu          cpuHistory
}

func strictJSON(input []byte, v any) error {
	if !utf8.Valid(input) {
		return errors.New("invalid UTF-8")
	}
	d := json.NewDecoder(bytes.NewReader(input))
	d.DisallowUnknownFields()
	if err := d.Decode(v); err != nil {
		return err
	}
	if d.Decode(new(any)) != io.EOF {
		return errors.New("trailing JSON")
	}
	return nil
}
func writeFrame(w io.Writer, data any) error {
	body, err := json.Marshal(data)
	if err != nil {
		return err
	}
	if len(body) > 1<<20 {
		return errResponseTooLong
	}
	if err = binary.Write(w, binary.BigEndian, uint32(len(body))); err != nil {
		return err
	}
	_, err = w.Write(body)
	return err
}

func writeResponse(w io.Writer, res response) error {
	err := writeFrame(w, res)
	if !errors.Is(err, errResponseTooLong) || res.Stats == nil {
		return err
	}
	// Keep basic stats available for Pods whose extended sample exceeds the wire limit.
	res.Stats.clearOptionalMetrics()
	for id, child := range res.Containers {
		child.clearOptionalMetrics()
		res.Containers[id] = child
	}
	return writeFrame(w, res)
}

func (s *server) serve(f *os.File) {
	defer f.Close()
	f.SetDeadline(time.Now().Add(3 * time.Second))
	if writeFrame(f, s.identity) != nil {
		return
	}
	var n uint32
	if binary.Read(f, binary.BigEndian, &n) != nil || n == 0 || n > 65536 {
		return
	}
	b := make([]byte, n)
	if _, err := io.ReadFull(f, b); err != nil {
		return
	}
	var req request
	var res response
	err := strictJSON(b, &req)
	podOp := req.Op == "podStats" || req.Op == "podMetrics"
	if err != nil {
		res = fail("invalid", err)
	} else if req.Version != 1 || !validID(req.ID) || (podOp && req.Container != "") || (!podOp && !validID(req.Container)) {
		res = fail("invalid", "invalid version or ID")
	} else if req.identity != s.identity {
		res = fail("identity", "guest launch identity mismatch")
	} else {
		f.SetDeadline(time.Time{})
		ctx, cancel := context.WithCancel(context.Background())
		defer cancel()
		// A host disconnect cancels this connection's exec, including descendants.
		if req.Op == "execStream" || req.Op == "attach" || req.Op == "portForward" {
			s.stream(f, req)
			return
		}
		if req.Op == "exec" {
			go func() { var scratch [1]byte; f.Read(scratch[:]); cancel() }()
		}
		res = s.dispatch(ctx, req)
	}
	res.Version, res.ID = 1, req.ID
	f.SetWriteDeadline(time.Now().Add(3 * time.Second))
	writeResponse(f, res)
}

func mount(typ, source, path string, flags uintptr) error {
	if err := os.MkdirAll(path, 0755); err != nil {
		return err
	}
	return unix.Mount(source, path, typ, flags, "")
}

func run() error {
	if len(os.Args) != 4 {
		return errors.New("invalid launch identity")
	}
	s := &server{identity: identity{1, os.Args[1], os.Args[2], os.Args[3]}, containers: map[string]*container{}, execSlots: make(chan struct{}, 24)}
	if !validID(s.Sandbox) || !validID(s.Nonce) || !validID(s.Boot) {
		return errors.New("invalid guest identity")
	}
	if err := unix.Prctl(unix.PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0); err != nil {
		return err
	}
	for _, fs := range []struct {
		typ, source, path string
		flags             uintptr
	}{
		{"devtmpfs", "devtmpfs", "/dev", unix.MS_NOSUID},
		{"sysfs", "sysfs", "/sys", unix.MS_NOSUID | unix.MS_NODEV | unix.MS_NOEXEC},
		{"cgroup2", "none", "/sys/fs/cgroup", unix.MS_NOSUID | unix.MS_NODEV | unix.MS_NOEXEC},
		{"tmpfs", "tmpfs", "/run", unix.MS_NOSUID | unix.MS_NODEV},
		{"virtiofs", "podfs", share, unix.MS_NOSUID | unix.MS_NODEV},
	} {
		if err := mount(fs.typ, fs.source, fs.path, fs.flags); err != nil && !errors.Is(err, unix.EBUSY) {
			return fmt.Errorf("mount %s: %w", fs.path, err)
		}
	}
	// Keep the workload parent observable even before the first container.
	if err := initWorkloadAccounting(); err != nil {
		log.Printf("guest cgroup accounting unavailable: %v", err)
	}
	// Pods using guest loopback only also support port forwarding.
	fdNet, err := unix.Socket(unix.AF_INET, unix.SOCK_DGRAM|unix.SOCK_CLOEXEC, 0)
	if err != nil {
		return err
	}
	iface, err := unix.NewIfreq("lo")
	if err == nil {
		err = unix.IoctlIfreq(fdNet, unix.SIOCGIFFLAGS, iface)
	}
	if err == nil && iface.Uint16()&unix.IFF_UP == 0 {
		iface.SetUint16(iface.Uint16() | unix.IFF_UP)
		err = unix.IoctlIfreq(fdNet, unix.SIOCSIFFLAGS, iface)
	}
	unix.Close(fdNet)
	if err != nil {
		return fmt.Errorf("enable guest loopback: %w", err)
	}
	if err := os.MkdirAll("/run/nautilus/shm", 0700); err != nil {
		return err
	}
	if err := unix.Mount("shm", "/run/nautilus/shm", "tmpfs", unix.MS_NOSUID|unix.MS_NODEV|unix.MS_NOEXEC, "mode=1777,size=64m"); err != nil {
		return err
	}
	if err := os.MkdirAll(runtimeRoot, 0700); err != nil {
		return err
	}
	if out, err := exec.Command("/bin/runc", "--version").CombinedOutput(); err != nil {
		return fmt.Errorf("runc: %s: %w", out, err)
	}
	fd, err := unix.Socket(unix.AF_VSOCK, unix.SOCK_STREAM|unix.SOCK_CLOEXEC, 0)
	if err != nil {
		return err
	}
	defer unix.Close(fd)
	if err = unix.Bind(fd, &unix.SockaddrVM{CID: unix.VMADDR_CID_ANY, Port: 4083}); err != nil {
		return err
	}
	if err = unix.Listen(fd, 16); err != nil {
		return err
	}
	slots := make(chan struct{}, 32)
	for {
		p, addr, err := unix.Accept4(fd, unix.SOCK_CLOEXEC|unix.SOCK_NONBLOCK)
		if errors.Is(err, unix.EINTR) {
			continue
		}
		if err != nil {
			return err
		}
		peer, ok := addr.(*unix.SockaddrVM)
		if !ok || peer.CID != unix.VMADDR_CID_HOST {
			unix.Close(p)
			continue
		}
		select {
		case slots <- struct{}{}:
			go func(file *os.File) { defer func() { <-slots }(); s.serve(file) }(os.NewFile(uintptr(p), "vsock"))
		default:
			unix.Close(p)
		}
	}
}

func main() {
	if err := run(); err != nil {
		log.Fatal(err)
	}
}
