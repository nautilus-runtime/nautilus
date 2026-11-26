package main

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"golang.org/x/sys/unix"
)

func TestBlockIOPreservesDeviceCountersAndAbsence(t *testing.T) {
	dir := t.TempDir()
	if got := readBlockIO(dir); got != nil {
		t.Fatalf("missing io.stat returned %+v", got)
	}
	stat := "8:0 rbytes=2048 wbytes=0 rios=2 also=12\n8:2 wbytes=512 wios=1 dbytes=0\n8:0 rbytes=256\n8:3 rios=invalid\nnot-a-device wbytes=60\n"
	if err := os.WriteFile(filepath.Join(dir, "io.stat"), []byte(stat), 0600); err != nil {
		t.Fatal(err)
	}
	got := readBlockIO(dir)
	if len(got) != 2 {
		t.Fatalf("expected two devices, got %+v", got)
	}
	first, second := got[0], got[1]
	if first.Major != 8 || first.Minor != 0 || first.Device == "" ||
		first.ReadBytes == nil || *first.ReadBytes != 2048 ||
		first.WriteBytes == nil || *first.WriteBytes != 0 ||
		first.ReadOps == nil || *first.ReadOps != 2 ||
		first.WriteOps != nil || first.DiscardBytes != nil {
		t.Fatalf("first device: %+v", first)
	}
	if second.Major != 8 || second.Minor != 2 || second.ReadBytes != nil ||
		second.WriteBytes == nil || *second.WriteBytes != 512 ||
		second.WriteOps == nil || *second.WriteOps != 1 ||
		second.DiscardBytes == nil || *second.DiscardBytes != 0 {
		t.Fatalf("second device: %+v", second)
	}
}

func TestBlockedFilesystemReadDoesNotQueueAnotherSampler(t *testing.T) {
	c := &container{id: "b9691ea3-8edf-45f5-b12f-27504bc35bc7", dir: t.TempDir()}
	config := filepath.Join(c.dir, "config.json")
	if err := unix.Mkfifo(config, 0600); err != nil {
		t.Fatal(err)
	}
	s := &server{}
	known := []*container{c}
	children := map[string]sample{c.id: {CPU: 42}}
	deadline := time.Now().Add(time.Second)
	done := make(chan struct{})
	go func() {
		s.sampleFilesystems(known, children, deadline)
		close(done)
	}()

	// Keep the reader blocked in a filesystem call past the response deadline.
	for {
		writer, err := unix.Open(config, unix.O_WRONLY|unix.O_NONBLOCK|unix.O_CLOEXEC, 0)
		if err == nil {
			defer unix.Close(writer)
			break
		}
		if err != unix.ENXIO || !time.Now().Before(deadline) {
			t.Fatalf("waiting for sampler: %v", err)
		}
		time.Sleep(time.Millisecond)
	}
	select {
	case <-done:
	case <-time.After(2 * time.Second):
		t.Fatal("response waited for a blocked storage worker")
	}
	if children[c.id].CPU != 42 || children[c.id].Filesystem != nil {
		t.Fatalf("basic sample was replaced: %+v", children[c.id])
	}
	second := make(chan struct{})
	go func() {
		s.sampleFilesystems(known, children, time.Now().Add(3*time.Second))
		close(second)
	}()
	select {
	case <-second:
	case <-time.After(time.Second):
		t.Fatal("second sampler waited behind the blocked worker")
	}
}

func TestRootFilesystemCountsOwnedAllocatedBlocks(t *testing.T) {
	root := t.TempDir()
	subdir := filepath.Join(root, "subdir")
	if err := os.Mkdir(subdir, 0700); err != nil {
		t.Fatal(err)
	}
	data := filepath.Join(subdir, "data")
	if err := os.WriteFile(data, make([]byte, 32<<10), 0600); err != nil {
		t.Fatal(err)
	}
	if err := os.Link(data, filepath.Join(root, "same-data")); err != nil {
		t.Fatal(err)
	}
	outside := filepath.Join(t.TempDir(), "unowned")
	if err := os.WriteFile(outside, make([]byte, 64<<10), 0600); err != nil {
		t.Fatal(err)
	}
	link := filepath.Join(root, "outside")
	if err := os.Symlink(outside, link); err != nil {
		t.Fatal(err)
	}
	var expected uint64
	for _, path := range []string{root, subdir, data, link} {
		var st unix.Stat_t
		if err := unix.Lstat(path, &st); err != nil {
			t.Fatal(err)
		}
		expected += uint64(st.Blocks) * 512
	}
	dir, err := os.Open(root)
	if err != nil {
		t.Fatal(err)
	}
	defer dir.Close()

	got := readRootFilesystem(dir, false, time.Now().Add(time.Second))
	if got == nil || got.UsageBytes == nil || *got.UsageBytes != expected {
		t.Fatalf("usage returned %+v, expected %d bytes", got, expected)
	}
	if got.CapacityBytes == nil || *got.CapacityBytes < expected || got.Inodes == nil || got.InodesFree == nil {
		t.Fatalf("backing filesystem geometry missing: %+v", got)
	}
	if readonly := readRootFilesystem(dir, true, time.Now().Add(time.Second)); readonly == nil || readonly.UsageBytes != nil {
		t.Fatalf("readonly sample: %+v", readonly)
	}

	expired := storageWalk{until: time.Now().Add(-time.Second), seen: make(map[uint64]bool)}
	if expired.directory(dir, 0) {
		t.Fatal("expired walk reported complete usage")
	}
}
