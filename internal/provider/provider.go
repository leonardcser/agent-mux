package provider

import (
	"strconv"
	"strings"
)

// ProcessTable holds a snapshot of the system process tree.
type ProcessTable struct {
	Children map[int][]int  // ppid -> child pids
	Comm     map[int]string // pid -> command basename
	Args     map[int]string // pid -> full command line args
}

var registry = map[string]bool{}

// Register adds an agent command name to the global registry.
func Register(cmd string) {
	registry[cmd] = true
}

// IsAgent returns true if the command matches a registered provider.
func IsAgent(cmd string) bool {
	return registry[cmd]
}

// Resolve returns the provider command name for a tmux pane. It first checks
// the direct command, then falls back to inspecting children of the shell
// process via the process table (handles cases like gemini running as "node").
func Resolve(cmd string, shellPID int, pt *ProcessTable) string {
	if registry[cmd] {
		return cmd
	}
	for _, childPID := range pt.Children[shellPID] {
		comm := pt.Comm[childPID]
		base := comm
		if idx := strings.LastIndex(comm, "/"); idx >= 0 {
			base = comm[idx+1:]
		}
		if registry[base] {
			return base
		}
		for arg := range strings.SplitSeq(pt.Args[childPID], " ") {
			if idx := strings.LastIndex(arg, "/"); idx >= 0 {
				arg = arg[idx+1:]
			}
			if registry[arg] {
				return arg
			}
		}
	}
	return ""
}

// ParseProcessTable builds a ProcessTable from raw `ps -eo pid,ppid,comm,args` output.
func ParseProcessTable(out string) ProcessTable {
	pt := ProcessTable{
		Children: make(map[int][]int),
		Comm:     make(map[int]string),
		Args:     make(map[int]string),
	}
	for line := range strings.SplitSeq(strings.TrimSpace(out), "\n") {
		fields := strings.Fields(line)
		if len(fields) < 3 {
			continue
		}
		pid, err1 := strconv.Atoi(fields[0])
		ppid, err2 := strconv.Atoi(fields[1])
		if err1 != nil || err2 != nil {
			continue
		}
		pt.Children[ppid] = append(pt.Children[ppid], pid)
		pt.Comm[pid] = fields[2]
		if len(fields) > 3 {
			pt.Args[pid] = strings.Join(fields[3:], " ")
		}
	}
	return pt
}
