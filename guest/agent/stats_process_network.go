package main

import (
	"bufio"
	"io"
	"io/fs"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

type processSample struct {
	Processes       *uint64 `json:"processes,omitempty"`
	FileDescriptors *uint64 `json:"fileDescriptors,omitempty"`
	Sockets         *uint64 `json:"sockets,omitempty"`
	Threads         *uint64 `json:"threads,omitempty"`
	ThreadsMax      *uint64 `json:"threadsMax,omitempty"`
}

func readProcessSample(group string, deadline time.Time) *processSample {
	if group != podCgroup && filepath.Dir(group) != podCgroup {
		return nil
	}
	return processMetricsFrom(group, "/sys/fs/cgroup", "/proc", deadline)
}

func pidCounter(group, file string) *uint64 {
	b, err := os.ReadFile(filepath.Join(group, file))
	if err != nil {
		return nil
	}
	value := strings.TrimSpace(string(b))
	if file == "pids.max" && value == "max" {
		return new(uint64)
	}
	n, err := strconv.ParseUint(value, 10, 64)
	if err != nil {
		return nil
	}
	return &n
}

func processMetricsFrom(group, cgroupRoot, procRoot string, deadline time.Time) *processSample {
	if !time.Now().Before(deadline) {
		return nil
	}
	rel, err := filepath.Rel(cgroupRoot, group)
	if err != nil || !filepath.IsLocal(rel) || rel == "." {
		return nil
	}
	s := &processSample{Threads: pidCounter(group, "pids.current"), ThreadsMax: pidCounter(group, "pids.max")}
	pids := make(map[string]string)
	if err := collectPIDGroups(group, "/"+rel, pids, deadline); err != nil {
		if s.Threads == nil && s.ThreadsMax == nil {
			return nil
		}
		return s
	}
	n := uint64(len(pids))
	s.Processes = &n
	var files, sockets uint64
	for pid, membership := range pids {
		if !time.Now().Before(deadline) {
			return s
		}
		f, sock, ok := descriptorCounts(filepath.Join(procRoot, pid), membership, deadline)
		if !ok {
			return s
		}
		files += f
		sockets += sock
	}
	s.FileDescriptors, s.Sockets = &files, &sockets
	return s
}

// cgroup.procs lists process IDs local to the group. A PID may appear more
// than once during a scan, including in two groups if the process moved.
func collectPIDGroups(group, membership string, pids map[string]string, deadline time.Time) error {
	file, err := os.Open(filepath.Join(group, "cgroup.procs"))
	if err != nil {
		return err
	}
	lines := bufio.NewScanner(file)
	for lines.Scan() {
		if !time.Now().Before(deadline) {
			file.Close()
			return os.ErrDeadlineExceeded
		}
		pid := lines.Text()
		if n, err := strconv.ParseUint(pid, 10, 32); err != nil || n == 0 {
			file.Close()
			return os.ErrInvalid
		}
		pids[pid] = membership
	}
	file.Close()
	if err := lines.Err(); err != nil {
		return err
	}
	dir, err := os.Open(group)
	if err != nil {
		return err
	}
	defer dir.Close()
	return eachEntry(dir, deadline, func(entry fs.DirEntry) error {
		if entry.IsDir() {
			return collectPIDGroups(filepath.Join(group, entry.Name()), membership+"/"+entry.Name(), pids, deadline)
		}
		return nil
	})
}

// Read bounded batches, including when one process has many open files.
func eachEntry(dir *os.File, deadline time.Time, visit func(fs.DirEntry) error) error {
	for time.Now().Before(deadline) {
		entries, err := dir.ReadDir(64)
		for _, entry := range entries {
			if !time.Now().Before(deadline) {
				return os.ErrDeadlineExceeded
			}
			if err := visit(entry); err != nil {
				return err
			}
		}
		if err == io.EOF {
			return nil
		}
		if err != nil {
			return err
		}
	}
	return os.ErrDeadlineExceeded
}

func descriptorCounts(pidDir, membership string, deadline time.Time) (uint64, uint64, bool) {
	// Pin the proc directory across both membership checks and FD reads.
	// A reused PID cannot redirect the second half of a sample to a new task.
	process, err := os.OpenRoot(pidDir)
	if err != nil {
		return 0, 0, false
	}
	defer process.Close()
	owns := func() bool {
		data, err := fs.ReadFile(process.FS(), "cgroup")
		if err == nil {
			for _, line := range strings.Split(string(data), "\n") {
				if line == "0::"+membership {
					return true
				}
			}
		}
		return false
	}
	if !owns() {
		return 0, 0, false
	}
	dir, err := process.Open("fd")
	if err != nil {
		return 0, 0, false
	}
	defer dir.Close()
	var files, sockets uint64
	err = eachEntry(dir, deadline, func(entry fs.DirEntry) error {
		var prefix [len("socket:[")]byte
		n, err := unix.Readlinkat(int(dir.Fd()), entry.Name(), prefix[:])
		if os.IsNotExist(err) {
			// This descriptor closed after readdir.
			return nil
		}
		if err != nil {
			return err
		}
		files++
		if string(prefix[:n]) == "socket:[" {
			sockets++
		}
		return nil
	})
	return files, sockets, err == nil && owns()
}

func readPodNetwork() map[string][8]uint64 {
	data, err := os.ReadFile("/proc/net/dev")
	if err != nil {
		return nil
	}
	return parsePodNetwork(string(data))
}

func parsePodNetwork(stat string) map[string][8]uint64 {
	interfaces := make(map[string][8]uint64)
rows:
	for _, line := range strings.Split(stat, "\n") {
		colon := strings.LastIndexByte(line, ':')
		if colon < 0 {
			continue
		}
		name := strings.TrimSpace(line[:colon])
		lower := strings.ToLower(name)
		if name == "" || strings.HasPrefix(lower, "lo") || strings.HasPrefix(lower, "veth") ||
			strings.HasPrefix(lower, "docker") || strings.HasPrefix(lower, "nerdctl") {
			continue
		}
		fields := strings.Fields(line[colon+1:])
		if len(fields) < 12 {
			continue
		}
		// /proc/net/dev lists receive bytes, packets, errors and drops at
		// 0..3 and the matching transmit counters at 8..11.
		var counters [8]uint64
		for i, index := range [...]int{0, 1, 2, 3, 8, 9, 10, 11} {
			n, err := strconv.ParseUint(fields[index], 10, 64)
			if err != nil {
				continue rows
			}
			counters[i] = n
		}
		interfaces[name] = counters
	}
	return interfaces
}
