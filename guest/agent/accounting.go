package main

import (
	"os"
	"path/filepath"
	"time"
)

func initWorkloadAccounting() error {
	rootControl := "/sys/fs/cgroup/cgroup.subtree_control"
	if err := os.WriteFile(rootControl, []byte("+memory"), 0600); err != nil {
		return err
	}
	if err := os.MkdirAll(podCgroup, 0755); err != nil {
		return err
	}
	podControl := filepath.Join(podCgroup, "cgroup.subtree_control")
	if err := os.WriteFile(podControl, []byte("+memory"), 0600); err != nil {
		return err
	}
	for _, controller := range []string{"+io", "+pids"} {
		if os.WriteFile(rootControl, []byte(controller), 0600) == nil {
			_ = os.WriteFile(podControl, []byte(controller), 0600)
		}
	}
	return nil
}

// Caller holds c.mu so the cgroup cannot be removed or its history replaced.
func (c *container) sample() (sample, error) {
	data, err := readSample(filepath.Join(podCgroup, c.id))
	if err == nil {
		c.cpu.update(&data, time.Now())
	}
	return data, err
}

func (s *server) podStats(extended bool) response {
	s.mu.Lock()
	known := make([]*container, 0, len(s.containers))
	for _, c := range s.containers {
		known = append(known, c)
	}
	s.mu.Unlock()

	deadline := time.Now().Add(250 * time.Millisecond)
	children := make(map[string]sample)
	for _, c := range known {
		c.mu.Lock()
		if c.current().State == "running" {
			data, err := c.sample()
			if err == nil && extended {
				group := filepath.Join(podCgroup, c.id)
				data.BlockIO = readBlockIO(group)
				data.Process = readProcessSample(group, deadline)
				if time.Now().Before(deadline) {
					data.Filesystem = readContainerFilesystem(c.id, c.dir)
				}
			}
			c.mu.Unlock()
			if err != nil {
				return fail("unavailable", err)
			}
			children[c.id] = data
		} else {
			c.mu.Unlock()
		}
	}
	s.statsMu.Lock()
	defer s.statsMu.Unlock()
	parent, err := readSample(podCgroup)
	if err != nil {
		return fail("unavailable", err)
	}
	s.cpu.update(&parent, time.Now())
	if extended {
		parent.BlockIO = readBlockIO(podCgroup)
		parent.Process = readProcessSample(podCgroup, deadline)
		parent.Network = readPodNetwork()
	}
	return response{OK: true, Stats: &parent, Containers: children}
}
