package main

import (
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/unix"
)

type user struct {
	UID    uint32   `json:"uid"`
	GID    uint32   `json:"gid"`
	Groups []uint32 `json:"additionalGids,omitempty"`
}
type process struct {
	OpenStdin   bool                `json:"-"`
	StdinOnce   bool                `json:"-"`
	OOMScoreAdj *int                `json:"oomScoreAdj,omitempty"`
	Terminal    bool                `json:"terminal"`
	User        *user               `json:"user"`
	Args        []string            `json:"args"`
	Env         []string            `json:"env"`
	Cwd         string              `json:"cwd"`
	Caps        map[string][]string `json:"capabilities"`
	NNP         bool                `json:"noNewPrivileges"`
}
type root struct {
	Path     string `json:"path"`
	Readonly bool   `json:"readonly"`
}
type mountSpec struct {
	Type        string   `json:"type"`
	Source      string   `json:"source"`
	Destination string   `json:"destination"`
	Options     []string `json:"options"`
}
type namespace struct {
	Type string `json:"type"`
}
type cpu struct {
	Shares *uint64 `json:"shares,omitempty"`
	Quota  *int64  `json:"quota,omitempty"`
	Period *uint64 `json:"period,omitempty"`
}
type memory struct {
	Limit *int64 `json:"limit,omitempty"`
}
type hugepage struct {
	PageSize string `json:"pageSize"`
	Limit    uint64 `json:"limit"`
}
type resources struct {
	Unified   map[string]string `json:"unified,omitempty"`
	Hugepages []hugepage        `json:"hugepageLimits,omitempty"`

	CPU    *cpu    `json:"cpu,omitempty"`
	Memory *memory `json:"memory,omitempty"`
}
type linuxSpec struct {
	MaskedPaths   []string    `json:"maskedPaths,omitempty"`
	ReadonlyPaths []string    `json:"readonlyPaths,omitempty"`
	Namespaces    []namespace `json:"namespaces"`
	Resources     *resources  `json:"resources,omitempty"`
}
type spec struct {
	Annotations map[string]string `json:"annotations,omitempty"`
	Version     string            `json:"ociVersion"`
	Root        root              `json:"root"`
	Process     process           `json:"process"`
	Linux       linuxSpec         `json:"linux"`
	Mounts      []mountSpec       `json:"mounts,omitempty"`
}

func absolute(s string) bool {
	return filepath.IsAbs(s) && filepath.Clean(s) == s && !strings.ContainsRune(s, 0)
}
func validArgv(a []string) bool {
	if len(a) == 0 || len(a) > 256 || a[0] == "" {
		return false
	}
	for _, v := range a {
		if strings.ContainsRune(v, 0) {
			return false
		}
	}
	return true
}

type exportKind int

const (
	bundleDir exportKind = iota
	configFile
	boundVolume
)

func secureExport(path string, kind exportKind) error {
	actual, err := filepath.EvalSymlinks(path)
	if err != nil {
		return err
	}
	if actual != path {
		return errors.New("symlink in export selector")
	}
	var st unix.Stat_t
	if err = unix.Lstat(path, &st); err != nil {
		return err
	}
	typ := st.Mode & unix.S_IFMT
	switch kind {
	case bundleDir:
		if st.Uid == 0 && typ == unix.S_IFDIR {
			return nil
		}
	case configFile:
		if st.Uid == 0 && typ == unix.S_IFREG && st.Nlink == 1 {
			return nil
		}
	case boundVolume:
		// The host pins this entry inside a sealed mount catalog. A CRI
		// source retains its own UID and hard-link count.
		if typ == unix.S_IFDIR || typ == unix.S_IFREG {
			return nil
		}
	}
	return errors.New("invalid export owner or type")
}

// The host publishes the bundle before create. No workload can mutate its rootfs
// yet. Reject symlinks along mount targets so aliases cannot shadow reserved mounts.
func checkTarget(root, target string) error {
	for current := filepath.Join(root, target); current != root; current = filepath.Dir(current) {
		st, err := os.Lstat(current)
		if errors.Is(err, os.ErrNotExist) {
			continue
		}
		if err != nil {
			return err
		}
		if st.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("symlink in mount target %s", target)
		}
	}
	return nil
}

func prepare(id, private string) (process, error) {
	var s spec
	bundle := filepath.Join(share, "containers", id)
	if err := secureExport(bundle, bundleDir); err != nil {
		return s.Process, err
	}
	config := filepath.Join(bundle, "config.json")
	if err := secureExport(config, configFile); err != nil {
		return s.Process, err
	}
	f, err := os.Open(config)
	if err != nil {
		return s.Process, err
	}
	defer f.Close()
	b, err := io.ReadAll(io.LimitReader(f, (256<<10)+1))
	if err != nil || len(b) > 256<<10 {
		return s.Process, errors.New("invalid config size")
	}
	if err = strictJSON(b, &s); err != nil {
		return s.Process, err
	}
	if (s.Version != "1.0.2" && s.Version != "1.1.0" && s.Version != "1.2.0") || s.Root.Path != "rootfs" || !validArgv(s.Process.Args) || s.Process.User == nil || !s.Process.NNP || !absolute(s.Process.Cwd) {
		return s.Process, errors.New("unsupported OCI process, root, or version")
	}
	for k, v := range s.Annotations {
		if (k != "io.nautilus.stdin" && k != "io.nautilus.stdinOnce") || (v != "true" && v != "false") {
			return s.Process, errors.New("invalid guest stdin annotation")
		}
	}
	s.Process.OpenStdin = s.Annotations["io.nautilus.stdin"] == "true"
	s.Process.StdinOnce = s.Annotations["io.nautilus.stdinOnce"] == "true"
	if s.Process.StdinOnce && !s.Process.OpenStdin {
		return s.Process, errors.New("stdinOnce requires stdin")
	}
	if adj := s.Process.OOMScoreAdj; adj != nil && (*adj < -1000 || *adj > 1000) {
		return s.Process, errors.New("invalid OOM score adjustment")
	}
	for _, paths := range [][]string{s.Linux.MaskedPaths, s.Linux.ReadonlyPaths} {
		for _, path := range paths {
			if !absolute(path) || path == "/" {
				return s.Process, errors.New("masked and readonly paths must be absolute container paths")
			}
		}
	}
	for _, v := range s.Process.Env {
		name, _, ok := strings.Cut(v, "=")
		if !ok || name == "" || strings.ContainsRune(v, 0) {
			return s.Process, errors.New("invalid process env")
		}
	}
	for k, v := range s.Process.Caps {
		if len(v) > 0 || (k != "bounding" && k != "effective" && k != "inheritable" && k != "permitted" && k != "ambient") {
			return s.Process, errors.New("only empty capabilities supported")
		}
	}
	s.Process.Caps = map[string][]string{"bounding": {}, "effective": {}, "inheritable": {}, "permitted": {}, "ambient": {}}
	ns := map[string]bool{}
	for _, n := range s.Linux.Namespaces {
		if ns[n.Type] || (n.Type != "mount" && n.Type != "pid" && n.Type != "ipc" && n.Type != "uts") {
			return s.Process, errors.New("unsupported namespace")
		}
		ns[n.Type] = true
	}
	if !ns["mount"] || !ns["pid"] {
		return s.Process, errors.New("private mount and PID namespaces required")
	}
	s.Root.Path = filepath.Join(bundle, "rootfs")
	if err = secureExport(s.Root.Path, bundleDir); err != nil {
		return s.Process, err
	}
	destinations := map[string]bool{}
	mounts := []mountSpec{
		{"proc", "proc", "/proc", []string{"nosuid", "nodev", "noexec"}},
		{"tmpfs", "tmpfs", "/dev", []string{"nosuid", "strictatime", "mode=755", "size=65536k"}},
		{"devpts", "devpts", "/dev/pts", []string{"newinstance", "nosuid", "noexec", "ptmxmode=0666", "mode=0620", "gid=5"}},
		{"mqueue", "mqueue", "/dev/mqueue", []string{"nosuid", "nodev", "noexec"}},
	}
	if ns["ipc"] {
		mounts = append(mounts, mountSpec{"tmpfs", "shm", "/dev/shm", []string{"nosuid", "nodev", "noexec", "mode=1777", "size=65536k"}})
	} else {
		mounts = append(mounts, mountSpec{"bind", "/run/nautilus/shm", "/dev/shm", []string{"bind", "nosuid", "nodev", "noexec"}})
	}
	for _, m := range s.Mounts {
		sub, ok := strings.CutPrefix(m.Source, "volumes/")
		if !ok || !validID(sub) || m.Type != "bind" || !absolute(m.Destination) || m.Destination == "/" || destinations[m.Destination] || len(m.Options) != 2 || m.Options[0] != "bind" || (m.Options[1] != "ro" && m.Options[1] != "rw") {
			return s.Process, errors.New("invalid volume mount")
		}
		m.Source = filepath.Join(share, m.Source)
		if err = secureExport(m.Source, boundVolume); err != nil {
			return s.Process, err
		}
		if m.Destination == "/dev/termination-log" {
			info, err := os.Stat(m.Source)
			if err != nil || !info.Mode().IsRegular() {
				return s.Process, errors.New("termination log must be a regular file")
			}
		} else {
			for _, reserved := range []string{"/proc", "/sys", "/dev"} {
				if m.Destination == reserved || strings.HasPrefix(m.Destination, reserved+"/") {
					return s.Process, fmt.Errorf("reserved mount target %s", m.Destination)
				}
			}
		}
		destinations[m.Destination] = true
		m.Options = append(m.Options, "nosuid", "nodev", "noexec")
		mounts = append(mounts, m)
	}
	if !destinations["/etc/resolv.conf"] {
		if _, err := os.Stat("/etc/resolv.conf"); err == nil {
			mounts = append(mounts, mountSpec{"bind", "/etc/resolv.conf", "/etc/resolv.conf", []string{"bind", "ro", "nodev", "noexec", "nosuid"}})
		}
	}
	for _, m := range mounts {
		if err := checkTarget(s.Root.Path, m.Destination); err != nil {
			return s.Process, err
		}
	}
	s.Mounts = mounts
	// Runtime-owned fields never come from the exported OCI input.
	linux := map[string]any{"namespaces": s.Linux.Namespaces, "cgroupsPath": "/nautilus/" + id,
		"maskedPaths":   append([]string{"/proc/cmdline", "/proc/kcore", "/proc/keys", "/proc/timer_list", "/proc/sysrq-trigger"}, s.Linux.MaskedPaths...),
		"readonlyPaths": append([]string{"/proc/sys", "/proc/bus", "/proc/fs", "/proc/irq", "/proc/asound"}, s.Linux.ReadonlyPaths...)}
	r := map[string]any{"devices": []map[string]any{{"allow": false, "access": "rwm"}}}
	if s.Linux.Resources != nil {
		for key, val := range s.Linux.Resources.Unified {
			if !(key == "memory.oom.group" && val == "1" || (key == "memory.swap.max" || key == "memory.low" || key == "memory.min") && val == "0") {
				return s.Process, errors.New("unsupported unified cgroup control")
			}
		}
		r["unified"] = s.Linux.Resources.Unified
		for _, p := range s.Linux.Resources.Hugepages {
			if (p.PageSize != "2MB" && p.PageSize != "1GB") || p.Limit != 0 {
				return s.Process, errors.New("unsupported guest hugepage limit")
			}
		}
		r["hugepageLimits"] = s.Linux.Resources.Hugepages
		if c := s.Linux.Resources.CPU; c != nil {
			if (c.Shares != nil && (*c.Shares < 2 || *c.Shares > 262144)) || (c.Period != nil && (*c.Period < 1000 || *c.Period > 1000000)) || (c.Quota != nil && *c.Quota != -1 && *c.Quota < 1000) {
				return s.Process, errors.New("invalid cpu resources")
			}
			r["cpu"] = c
		}
		if m := s.Linux.Resources.Memory; m != nil {
			if m.Limit != nil && *m.Limit != -1 && *m.Limit <= 0 {
				return s.Process, errors.New("invalid memory limit")
			}
			r["memory"] = m
		}
	}
	linux["resources"] = r
	data, err := json.Marshal(map[string]any{"ociVersion": s.Version, "root": s.Root, "process": s.Process, "mounts": s.Mounts, "linux": linux})
	if err != nil {
		return s.Process, err
	}
	if err = os.Mkdir(private, 0700); err != nil {
		return s.Process, err
	}
	if err = os.WriteFile(filepath.Join(private, "config.json"), data, 0600); err != nil {
		return s.Process, fmt.Errorf("write private OCI bundle: %w", err)
	}
	return s.Process, nil
}
