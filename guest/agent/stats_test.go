package main

import (
	"os"
	"path/filepath"
	"testing"
	"time"
)

func TestWorkloadCgroupSample(t *testing.T) {
	dir := t.TempDir()
	write := func(file, data string) {
		t.Helper()
		if err := os.WriteFile(filepath.Join(dir, file), []byte(data), 0600); err != nil {
			t.Fatal(err)
		}
	}
	write("cpu.stat", "usage_usec 1275\nuser_usec 1000\nsystem_usec 275\n")
	write("memory.current", "4096\n")
	write("memory.stat", "anon 1024\nswapcached 256\ninactive_file 1536\npgfault 32\npgmajfault 2\n")
	write("memory.max", "8192\n")
	s, err := readSample(dir)
	if err != nil || s.CPU != 1275000 || s.Usage != 4096 || s.WorkingSet != 2560 ||
		s.RSS != 1280 || s.Faults != 30 || s.MajorFaults != 2 || s.Available == nil || *s.Available != 5632 || s.Timestamp <= 0 {
		t.Fatalf("sample = %+v, err = %v", s, err)
	}
	write("memory.max", "max\n")
	write("memory.current", "512\n")
	s, err = readSample(dir)
	if err != nil || s.WorkingSet != 0 || s.Available != nil {
		t.Fatalf("unlimited sample = %+v, err = %v", s, err)
	}
	write("cpu.stat", "usage_usec 18446744073709551615\n")
	if _, err := readSample(dir); err == nil {
		t.Fatal("accepted CPU usage that overflows nanoseconds")
	}
	write("cpu.stat", "usage_usec 11\n")
	write("memory.stat", "anon 0\n")
	if _, err := readSample(dir); err == nil {
		t.Fatal("accepted incomplete accounting as zero usage")
	}
}

func TestCPURateNeedsTwoSeparatedSamples(t *testing.T) {
	var history cpuHistory
	s := sample{CPU: 500000}
	start := time.Now()
	history.update(&s, start)
	if s.NanoCores != nil {
		t.Fatal("unexpected rate from a single observation")
	}
	s.CPU += 500000000
	history.update(&s, start.Add(2*time.Second))
	if s.NanoCores == nil || *s.NanoCores != 250000000 {
		t.Fatalf("expected 250 millicores, got %v", s.NanoCores)
	}
	history.update(&s, start.Add(2100*time.Millisecond))
	if *s.NanoCores != 250000000 {
		t.Fatal("short interval discarded the measured rate")
	}
}
