package main

import (
	"bufio"
	"bytes"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"math"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"golang.org/x/sys/unix"
)

type filesystemSample struct {
	Device        string  `json:"device"`
	CapacityBytes *uint64 `json:"capacityBytes,omitempty"`
	Inodes        *uint64 `json:"inodes,omitempty"`
	InodesFree    *uint64 `json:"inodesFree,omitempty"`
	UsageBytes    *uint64 `json:"usageBytes,omitempty"`
}

type blockIOSample struct {
	Device       string  `json:"device"`
	Major        uint32  `json:"major"`
	Minor        uint32  `json:"minor"`
	ReadBytes    *uint64 `json:"readBytes,omitempty"`
	WriteBytes   *uint64 `json:"writeBytes,omitempty"`
	DiscardBytes *uint64 `json:"discardBytes,omitempty"`
	ReadOps      *uint64 `json:"readOps,omitempty"`
	WriteOps     *uint64 `json:"writeOps,omitempty"`
}

func readBlockIO(cgroupDir string) []blockIOSample {
	file, err := os.Open(filepath.Join(cgroupDir, "io.stat"))
	if err != nil {
		return nil
	}
	defer file.Close()
	const limit = 64 << 10
	data, err := io.ReadAll(io.LimitReader(file, limit+1))
	if err != nil || len(data) > limit {
		return nil
	}
	return parseBlockIO(data)
}

func parseBlockIO(data []byte) []blockIOSample {
	var result []blockIOSample
	seen := make(map[[2]uint32]bool)
	scan := bufio.NewScanner(bytes.NewReader(data))
	for scan.Scan() {
		fields := strings.Fields(scan.Text())
		if len(fields) < 2 {
			continue
		}
		major, minor, ok := strings.Cut(fields[0], ":")
		if !ok {
			continue
		}
		majorID, err := strconv.ParseUint(major, 10, 32)
		if err != nil {
			continue
		}
		minorID, err := strconv.ParseUint(minor, 10, 32)
		if err != nil {
			continue
		}
		key := [2]uint32{uint32(majorID), uint32(minorID)}
		if seen[key] {
			continue
		}
		s := blockIOSample{Major: key[0], Minor: key[1]}
		for _, field := range fields[1:] {
			source, value, ok := strings.Cut(field, "=")
			if !ok {
				continue
			}
			n, err := strconv.ParseUint(value, 10, 64)
			if err != nil {
				continue
			}
			switch source {
			case "rbytes":
				s.ReadBytes = &n
			case "wbytes":
				s.WriteBytes = &n
			case "dbytes":
				s.DiscardBytes = &n
			case "rios":
				s.ReadOps = &n
			case "wios":
				s.WriteOps = &n
			}
		}
		if s.ReadBytes == nil && s.WriteBytes == nil && s.DiscardBytes == nil && s.ReadOps == nil && s.WriteOps == nil {
			continue
		}
		seen[key] = true
		id := fmt.Sprintf("%d:%d", s.Major, s.Minor)
		s.Device = id
		if info, err := os.ReadFile(filepath.Join("/sys/dev/block", id, "uevent")); err == nil {
			for _, line := range strings.Split(string(info), "\n") {
				if name, ok := strings.CutPrefix(line, "DEVNAME="); ok && name != "" {
					s.Device = "/dev/" + name
					break
				}
			}
		}
		result = append(result, s)
	}
	if scan.Err() != nil {
		return nil
	}
	return result
}

const storageLookup = unix.RESOLVE_BENEATH | unix.RESOLVE_NO_SYMLINKS | unix.RESOLVE_NO_XDEV
const storageDirectory = unix.O_RDONLY | unix.O_DIRECTORY | unix.O_CLOEXEC

func readContainerFilesystem(id string, privateBundle string, deadline time.Time) *filesystemSample {
	if !validID(id) {
		return nil
	}
	path := filepath.Join(share, "containers", id, "rootfs")
	data, err := os.ReadFile(filepath.Join(privateBundle, "config.json"))
	if err != nil {
		return nil
	}
	var config struct {
		Root *root `json:"root"`
	}
	if json.Unmarshal(data, &config) != nil || config.Root == nil || config.Root.Path != path {
		return nil
	}

	shared, err := unix.Open(share, storageDirectory|unix.O_NOFOLLOW, 0)
	if err != nil {
		return nil
	}
	defer unix.Close(shared)
	fd, err := unix.Openat2(shared, filepath.Join("containers", id, "rootfs"), &unix.OpenHow{
		Flags:   storageDirectory,
		Resolve: storageLookup,
	})
	if err != nil {
		return nil
	}
	dir := os.NewFile(uintptr(fd), path)
	defer dir.Close()
	return readRootFilesystem(dir, config.Root.Readonly, deadline)
}

func readRootFilesystem(dir *os.File, readonly bool, deadline time.Time) *filesystemSample {
	s := filesystemSample{Device: "virtiofs:podfs"}
	var stats unix.Statfs_t
	if unix.Fstatfs(int(dir.Fd()), &stats) == nil {
		unit := stats.Frsize
		if unit <= 0 {
			unit = stats.Bsize
		}
		if unit > 0 && stats.Blocks > 0 && stats.Blocks <= math.MaxUint64/uint64(unit) {
			capacity := stats.Blocks * uint64(unit)
			s.CapacityBytes = &capacity
		}
		if stats.Files > 0 {
			s.Inodes = &stats.Files
			s.InodesFree = &stats.Ffree
		}
	}
	if !readonly {
		// VM images are materialized as a private rootfs, including the image
		// blocks. Volume mounts and guest-visible pseudo filesystems are outside it.
		until := time.Now().Add(100 * time.Millisecond)
		if deadline.Before(until) {
			until = deadline
		}
		walk := storageWalk{until: until, seen: make(map[uint64]bool)}
		if walk.directory(dir, 0) {
			s.UsageBytes = &walk.total
		}
	}
	if s.CapacityBytes == nil && s.Inodes == nil && s.UsageBytes == nil {
		return nil
	}
	return &s
}

type storageWalk struct {
	until time.Time
	seen  map[uint64]bool
	count int
	total uint64
}

func (w *storageWalk) account(st *unix.Stat_t) bool {
	if w.seen[st.Ino] {
		return true
	}
	w.seen[st.Ino] = true
	if st.Blocks < 0 || uint64(st.Blocks) > (math.MaxUint64-w.total)/512 {
		return false
	}
	w.total += uint64(st.Blocks) * 512
	return true
}

func (w *storageWalk) directory(dir *os.File, depth int) bool {
	if depth > 64 || time.Now().After(w.until) {
		return false
	}
	var st unix.Stat_t
	fd := int(dir.Fd())
	if unix.Fstat(fd, &st) != nil || !w.account(&st) {
		return false
	}
	for {
		names, err := dir.Readdirnames(128)
		if err != nil && !errors.Is(err, io.EOF) {
			return false
		}
		for _, name := range names {
			w.count++
			if w.count > 16384 || time.Now().After(w.until) || !w.entry(fd, name, depth) {
				return false
			}
		}
		if errors.Is(err, io.EOF) {
			return true
		}
	}
}

func (w *storageWalk) entry(parent int, name string, depth int) bool {
	fd, err := unix.Openat2(parent, name, &unix.OpenHow{
		Flags:   unix.O_PATH | unix.O_NOFOLLOW | unix.O_CLOEXEC,
		Resolve: storageLookup,
	})
	if err != nil {
		// A deleted entry has no charge. A foreign mount belongs to its own store.
		return errors.Is(err, unix.ENOENT) || errors.Is(err, unix.EXDEV)
	}
	defer unix.Close(fd)
	var st unix.Stat_t
	if unix.Fstat(fd, &st) != nil {
		return false
	}
	if st.Mode&unix.S_IFMT != unix.S_IFDIR {
		return w.account(&st)
	}
	childFD, err := unix.Openat2(fd, ".", &unix.OpenHow{
		Flags:   storageDirectory,
		Resolve: storageLookup,
	})
	if err != nil {
		return false
	}
	child := os.NewFile(uintptr(childFD), name)
	defer child.Close()
	return w.directory(child, depth+1)
}
