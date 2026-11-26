package main

import (
	"encoding/json"
	"maps"
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestProcessMetricsFromOwnedSubtree(t *testing.T) {
	root := t.TempDir()
	cg := filepath.Join(root, "cgroup")
	group := filepath.Join(cg, "nautilus", "work")
	proc := filepath.Join(root, "proc")
	write := func(path, value string) {
		t.Helper()
		if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
			t.Fatal(err)
		}
		if err := os.WriteFile(path, []byte(value), 0644); err != nil {
			t.Fatal(err)
		}
	}
	write(filepath.Join(group, "cgroup.procs"), "11\n11\n")
	write(filepath.Join(group, "sub", "cgroup.procs"), "12\n")
	write(filepath.Join(cg, "nautilus", "peer", "cgroup.procs"), "13\n")
	write(filepath.Join(group, "pids.current"), "5\n")
	write(filepath.Join(group, "pids.max"), "max\n")
	write(filepath.Join(proc, "11", "cgroup"), "0::/nautilus/work\n")
	write(filepath.Join(proc, "12", "cgroup"), "0::/nautilus/work/sub\n")
	for path, target := range map[string]string{"11/fd/4": "/file", "12/fd/5": "socket:[123]", "12/fd/6": "socket:[123]"} {
		path = filepath.Join(proc, path)
		if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
			t.Fatal(err)
		}
		if err := os.Symlink(target, path); err != nil {
			t.Fatal(err)
		}
	}
	collect := func() *processSample {
		return processMetricsFrom(group, cg, proc, time.Now().Add(time.Second))
	}
	check := func(want map[string]uint64) {
		t.Helper()
		raw, err := json.Marshal(collect())
		if err != nil {
			t.Fatal(err)
		}
		var got map[string]uint64
		if err := json.Unmarshal(raw, &got); err != nil || !maps.Equal(got, want) {
			t.Fatalf("got %s (%v), want %v", raw, err, want)
		}
	}
	check(map[string]uint64{"processes": 2, "fileDescriptors": 3, "sockets": 2, "threads": 5, "threadsMax": 0})
	// A moved or reused PID must not charge a neighbor's descriptors.
	write(filepath.Join(proc, "12", "cgroup"), "0::/nautilus/peer\n")
	write(filepath.Join(group, "pids.max"), "64\n")
	check(map[string]uint64{"processes": 2, "threads": 5, "threadsMax": 64})
	// A task can exit between reading cgroup.procs and opening /proc/PID.
	if err := os.RemoveAll(filepath.Join(proc, "12")); err != nil {
		t.Fatal(err)
	}
	check(map[string]uint64{"processes": 2, "threads": 5, "threadsMax": 64})
	if err := os.Remove(filepath.Join(group, "sub", "cgroup.procs")); err != nil {
		t.Fatal(err)
	}
	check(map[string]uint64{"threads": 5, "threadsMax": 64})
	write(filepath.Join(group, "sub", "cgroup.procs"), "")
	write(filepath.Join(group, "cgroup.procs"), "")
	write(filepath.Join(group, "pids.current"), "0\n")
	check(map[string]uint64{"processes": 0, "fileDescriptors": 0, "sockets": 0, "threads": 0, "threadsMax": 64})
	for _, f := range []string{"pids.current", "pids.max"} {
		if err := os.Remove(filepath.Join(group, f)); err != nil {
			t.Fatal(err)
		}
	}
	check(map[string]uint64{"processes": 0, "fileDescriptors": 0, "sockets": 0})
	if data := processMetricsFrom(group, cg, proc, time.Now().Add(-time.Second)); data != nil {
		t.Fatalf("expired budget returned %+v", data)
	}
}

func TestPodNetworkMetricsSelectGuestInterfaces(t *testing.T) {
	rows := " Inter-| Receive | Transmit\n"
	for _, name := range []string{"eth0", "enp0s1:1", "LO", "lo0", "veth123", "DoCkEr0", "nerdctl0"} {
		rows += name + ": 11 12 13 14 99 99 99 99 21 22 23 24 99 99 99 99\n"
	}
	rows += "bad: 1 2 3\nmalformed: 1 2 3 4 5 6 7 8 9 10 ? 12\n"
	net := parsePodNetwork(rows)
	want := [8]uint64{11, 12, 13, 14, 21, 22, 23, 24}
	if len(net) != 2 || net["eth0"] != want || net["enp0s1:1"] != want {
		t.Fatalf("unexpected guest counters: %v", net)
	}
}
