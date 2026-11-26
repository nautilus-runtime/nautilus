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

	children := make(map[string]sample)
	var running []*container
	for _, c := range known {
		c.mu.Lock()
		if c.current().State == "running" {
			data, err := c.sample()
			c.mu.Unlock()
			if err != nil {
				return fail("unavailable", err)
			}
			children[c.id] = data
			if extended {
				running = append(running, c)
			}
		} else {
			c.mu.Unlock()
		}
	}
	s.statsMu.Lock()
	parent, err := readSample(podCgroup)
	if err == nil {
		s.cpu.update(&parent, time.Now())
	}
	s.statsMu.Unlock()
	if err != nil {
		return fail("unavailable", err)
	}
	if extended {
		deadline := time.Now().Add(250 * time.Millisecond)
		parent.BlockIO = readBlockIO(podCgroup)
		parent.Process = readProcessSample(podCgroup, deadline)
		parent.Network = readPodNetwork()
		for _, c := range running {
			data := children[c.id]
			group := filepath.Join(podCgroup, c.id)
			data.BlockIO = readBlockIO(group)
			data.Process = readProcessSample(group, deadline)
			children[c.id] = data
		}
		s.sampleFilesystems(running, children, deadline)
		s.mu.Lock()
		for _, c := range running {
			if s.containers[c.id] != c {
				delete(children, c.id)
			}
		}
		s.mu.Unlock()
	}
	return response{OK: true, Stats: &parent, Containers: children}
}

func (s *server) sampleFilesystems(running []*container, children map[string]sample, deadline time.Time) {
	if len(running) == 0 || !time.Now().Before(deadline) || !s.filesystemMu.TryLock() {
		return
	}
	// A stalled virtio-fs syscall must not hold a lifecycle lock or occupy more
	// than one worker per guest. A later request can still return local stats.
	type reading struct {
		id         string
		filesystem *filesystemSample
	}
	results := make(chan reading, len(running))
	go func() {
		defer close(results)
		defer s.filesystemMu.Unlock()
		for _, c := range running {
			if !time.Now().Before(deadline) {
				return
			}
			if filesystem := readContainerFilesystem(c.id, c.dir, deadline); filesystem != nil {
				results <- reading{c.id, filesystem}
			}
		}
	}()
	timer := time.NewTimer(time.Until(deadline))
	defer timer.Stop()
	for {
		select {
		case result, ok := <-results:
			if !ok {
				return
			}
			data := children[result.id]
			data.Filesystem = result.filesystem
			children[result.id] = data
		case <-timer.C:
			return
		}
	}
}
