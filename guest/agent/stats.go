package main

import (
	"fmt"
	"math"
	"math/bits"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"time"
)

const podCgroup = "/sys/fs/cgroup/nautilus"

// A sample contains only counters from the selected cgroup and its descendants.
type sample struct {
	Timestamp      int64                        `json:"timestamp"`
	CPU            uint64                       `json:"cpuUsageNanos"`
	NanoCores      *uint64                      `json:"usageNanoCores,omitempty"`
	Usage          uint64                       `json:"memoryUsageBytes"`
	WorkingSet     uint64                       `json:"workingSetBytes"`
	Available      *uint64                      `json:"availableBytes,omitempty"`
	RSS            uint64                       `json:"rssBytes"`
	Faults         uint64                       `json:"pageFaults"`
	MajorFaults    uint64                       `json:"majorPageFaults"`
	CgroupCounters map[string]map[string]uint64 `json:"cgroupCounters,omitempty"`
	Filesystem     *filesystemSample            `json:"filesystem,omitempty"`
	BlockIO        []blockIOSample              `json:"blockIo,omitempty"`
	Process        *processSample               `json:"process,omitempty"`
	Network        map[string][8]uint64         `json:"network,omitempty"`
}

func counters(path string, names ...string) (map[string]uint64, error) {
	data, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	values := make(map[string]uint64)
	for _, line := range strings.Split(strings.TrimSpace(string(data)), "\n") {
		key, number, _ := strings.Cut(line, " ")
		n, err := strconv.ParseUint(strings.TrimSpace(number), 10, 64)
		if err != nil {
			return nil, fmt.Errorf("%s %s: %w", path, key, err)
		}
		values[key] = n
	}
	for _, name := range names {
		if _, ok := values[name]; !ok {
			return nil, fmt.Errorf("missing %s in %s", name, path)
		}
	}
	return values, nil
}

func difference(total, part uint64) uint64 {
	if total <= part {
		return 0
	}
	return total - part
}

func selectedCounters(values map[string]uint64, keys ...string) map[string]uint64 {
	selected := make(map[string]uint64, len(keys))
	for _, key := range keys {
		if value, ok := values[key]; ok {
			selected[key] = value
		}
	}
	return selected
}

func readSample(dir string) (sample, error) {
	var s sample
	cpu, err := counters(filepath.Join(dir, "cpu.stat"), "usage_usec")
	if err != nil {
		return s, err
	}
	if cpu["usage_usec"] > math.MaxUint64/1000 {
		return s, fmt.Errorf("CPU nanoseconds overflow")
	}
	s.CPU = cpu["usage_usec"] * 1000
	current, err := os.ReadFile(filepath.Join(dir, "memory.current"))
	if err != nil {
		return s, err
	}
	if s.Usage, err = strconv.ParseUint(strings.TrimSpace(string(current)), 10, 64); err != nil {
		return s, err
	}
	mem, err := counters(filepath.Join(dir, "memory.stat"), "inactive_file", "anon", "pgfault", "pgmajfault")
	if err != nil {
		return s, err
	}
	s.WorkingSet = difference(s.Usage, mem["inactive_file"])
	// The shipped initramfs never activates swap. Newer kernels also expose
	// swapcached explicitly, so include those resident pages when available.
	rss, carry := bits.Add64(mem["anon"], mem["swapcached"], 0)
	if carry != 0 {
		return s, fmt.Errorf("RSS bytes overflow")
	}
	s.RSS = rss
	s.MajorFaults = mem["pgmajfault"]
	s.Faults = difference(mem["pgfault"], s.MajorFaults)
	s.CgroupCounters = map[string]map[string]uint64{
		"cpu.stat":    selectedCounters(cpu, "user_usec", "system_usec", "nr_periods", "nr_throttled", "throttled_usec"),
		"memory.stat": selectedCounters(mem, "file", "anon", "kernel", "file_mapped", "pgfault", "pgmajfault"),
	}
	maximum, err := os.ReadFile(filepath.Join(dir, "memory.max"))
	if err != nil {
		return s, err
	}
	if val := strings.TrimSpace(string(maximum)); val != "max" {
		limit, err := strconv.ParseUint(val, 10, 64)
		if err != nil {
			return s, err
		}
		available := difference(limit, s.WorkingSet)
		s.Available = &available
	}
	s.Timestamp = time.Now().UnixNano()
	return s, nil
}

type cpuHistory struct {
	at   time.Time
	cpu  uint64
	rate *uint64
}

// Rate is available after two observations at least a second apart. Closer
// callers share that rate without shortening the measurement interval.
func (h *cpuHistory) update(s *sample, now time.Time) {
	if h.at.IsZero() || s.CPU < h.cpu {
		*h = cpuHistory{at: now, cpu: s.CPU}
		return
	}
	if elapsed := now.Sub(h.at); elapsed >= time.Second {
		hi, lo := bits.Mul64(s.CPU-h.cpu, uint64(time.Second))
		h.rate = nil
		if hi < uint64(elapsed) {
			rate, _ := bits.Div64(hi, lo, uint64(elapsed))
			h.rate = &rate
		}
		h.at, h.cpu = now, s.CPU
	}
	s.NanoCores = h.rate
}
