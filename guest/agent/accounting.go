package main

import (
	"os"
	"path/filepath"
	"time"
)

func initWorkloadAccounting() error {
	if err := os.WriteFile("/sys/fs/cgroup/cgroup.subtree_control", []byte("+memory"), 0600); err != nil {
		return err
	}
	if err := os.MkdirAll(podCgroup, 0755); err != nil {
		return err
	}
	return os.WriteFile(filepath.Join(podCgroup, "cgroup.subtree_control"), []byte("+memory"), 0600)
}

// Caller holds c.mu so the cgroup cannot be removed or its history replaced.
func (c *container) sample() (sample, error) {
	data, err := readSample(filepath.Join(podCgroup, c.id))
	if err == nil {
		c.cpu.update(&data, time.Now())
	}
	return data, err
}

func (s *server) podStats() response {
	s.mu.Lock()
	known := make([]*container, 0, len(s.containers))
	for _, c := range s.containers {
		known = append(known, c)
	}
	s.mu.Unlock()

	children := make(map[string]sample)
	for _, c := range known {
		c.mu.Lock()
		if c.current().State == "running" {
			data, err := c.sample()
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
	return response{OK: true, Stats: &parent, Containers: children}
}
