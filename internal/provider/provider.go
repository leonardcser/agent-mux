package provider

import (
	"strconv"
	"strings"
)

// ProcessTable holds a snapshot of the system process tree.
type ProcessTable struct {
	Children map[int][]int  // ppid -> child pids
	Comm     map[int]string // pid -> executable/command string
	Args     map[int]string // pid -> full command line
}

var registry = map[string]bool{}

func init() {
	for _, cmd := range []string{"smelt", "claude", "codex", "gemini", "opencode", "ralph", "kimi"} {
		Register(cmd)
	}
}

// Register adds an agent command name to the global registry.
func Register(cmd string) {
	normalized := normalize(cmd)
	if normalized != "" {
		registry[normalized] = true
	}
}

// IsAgent returns true if the command matches a registered provider.
func IsAgent(cmd string) bool {
	return resolveRegistered(cmd) != ""
}

// Resolve returns the provider command name for a tmux pane. It first checks
// the direct command, then falls back to inspecting children of the shell
// process via the process table (handles cases like gemini running as "node").
func Resolve(cmd string, shellPID int, pt *ProcessTable) string {
	if matched := resolveRegistered(cmd); matched != "" {
		return matched
	}
	for _, childPID := range pt.Children[shellPID] {
		comm := pt.Comm[childPID]
		if matched := resolveRegistered(comm); matched != "" {
			return matched
		}
		args := pt.Args[childPID]
		if matched := resolveRegistered(args); matched != "" {
			return matched
		}
		for arg := range strings.SplitSeq(args, " ") {
			if idx := strings.LastIndex(arg, "/"); idx >= 0 {
				arg = arg[idx+1:]
			}
			if matched := resolveRegistered(arg); matched != "" {
				return matched
			}
		}
	}
	return ""
}

func normalize(cmd string) string {
	return strings.ToLower(strings.TrimSpace(cmd))
}

func resolveRegistered(cmd string) string {
	normalized := normalize(cmd)
	if normalized == "" {
		return ""
	}
	for registered := range registry {
		if strings.Contains(normalized, registered) {
			return registered
		}
	}
	if idx := strings.LastIndex(normalized, "/"); idx >= 0 {
		base := normalized[idx+1:]
		for registered := range registry {
			if strings.Contains(base, registered) {
				return registered
			}
		}
	}
	return ""
}

// ParseProcessTable builds a ProcessTable from raw `ps -eo pid=,ppid=,command=` output.
func ParseProcessTable(out string) ProcessTable {
	pt := ProcessTable{
		Children: make(map[int][]int),
		Comm:     make(map[int]string),
		Args:     make(map[int]string),
	}
	for line := range strings.SplitSeq(strings.TrimSpace(out), "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) < 3 {
			continue
		}
		pid, err1 := strconv.Atoi(fields[0])
		ppid, err2 := strconv.Atoi(fields[1])
		if err1 != nil || err2 != nil {
			continue
		}
		cmdline := strings.TrimSpace(strings.TrimPrefix(line, fields[0]))
		cmdline = strings.TrimSpace(strings.TrimPrefix(cmdline, fields[1]))
		pt.Children[ppid] = append(pt.Children[ppid], pid)
		pt.Args[pid] = cmdline
		pt.Comm[pid] = fields[2]
	}
	return pt
}
